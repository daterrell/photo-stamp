//! The `/v1` API (see `photo_stamp_proto::api`): device keys checked against the [`Store`], places
//! named by the [`Gazetteer`] over the server's full data.
//!
//! The place data can be replaced underneath a running server (`photo-stamp-server import`): a
//! new `places.sqlite` is noticed and loaded, and a newly mirrored Overture release is picked up
//! by the reader itself.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use photo_stamp_http::{Request, Response};
use photo_stamp_proto::GpsPoint;
use photo_stamp_proto::api::{BatchReply, BatchRequest, MAX_BATCH, Me, MeUpdate, PlaceReply, PlaceRequest};
use photo_stamp_proto::places::{FILE_NAME, PlacesDb, TOWNS_FILE_NAME};

use crate::gazetteer::Gazetteer;
use crate::overture::Overture;
use crate::store::{Caller, Store};
use crate::updates::Updates;

/// Where the server keeps everything (`/data` in the container).
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn app_db(&self) -> PathBuf {
        self.root.join("app.sqlite")
    }
    pub fn places(&self) -> PathBuf {
        self.root.join(FILE_NAME)
    }
    pub fn towns(&self) -> PathBuf {
        self.root.join(TOWNS_FILE_NAME)
    }
    /// Overture indexes and the range cache for files not (yet) mirrored.
    pub fn overture(&self) -> PathBuf {
        self.root.join("overture")
    }
    pub fn mirror(&self) -> PathBuf {
        self.root.join("mirror")
    }
}

/// Place data as loaded from disk.
pub struct Data {
    pub gazetteer: Gazetteer,
    places_mtime: Option<SystemTime>,
}

impl Data {
    /// `offline`: don't read Overture remotely for files missing from the mirror.
    pub fn load(layout: &Layout, offline: bool) -> anyhow::Result<Self> {
        let path = layout.places();
        let places = PlacesDb::open(&path)?;
        let overture = if offline && !layout.mirror().is_dir() {
            None
        } else {
            Some(Overture::new(&layout.overture())?.with_mirror(&layout.mirror()))
        };
        Ok(Self { gazetteer: Gazetteer::new(places, overture), places_mtime: mtime(&path) })
    }

    pub fn data_version(&self) -> String {
        let release =
            self.gazetteer.overture_health().and_then(|o| o.release).unwrap_or_else(|| "none".into());
        format!("{release}/{}", self.gazetteer.places_built().unwrap_or_else(|| "unknown".into()))
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

pub struct Service {
    pub store: Store,
    /// `None` until the first import has produced place data.
    data: RwLock<Option<Arc<Data>>>,
    layout: Layout,
    offline: bool,
    /// Client updates (`None`: not set up on this server).
    updates: Option<Updates>,
}

/// While there's no place data, check for it this often.
const WAITING_CHECK_EVERY: Duration = Duration::from_secs(10);

/// Lookups before the first import has produced place data. Clients treat it like an unreachable
/// server: photos wait.
fn still_loading() -> Response {
    Response::error(503, "the place data is still loading (the first import takes a while); try again later")
        .with_header("Retry-After", "60")
}

fn bearer(req: &Request) -> Option<&str> {
    req.header("authorization").and_then(|h| h.strip_prefix("Bearer ")).map(str::trim)
}

impl Service {
    /// `data`: `None` while there's no place data yet (a new server before its first import): the
    /// server answers anyway, with 503 for lookups, and picks the data up when it appears.
    pub fn new(
        store: Store,
        data: Option<Data>,
        layout: Layout,
        offline: bool,
        updates: Option<Updates>,
    ) -> Self {
        Self { store, data: RwLock::new(data.map(Arc::new)), layout, offline, updates }
    }

    /// The place data, if it's loaded.
    pub fn data(&self) -> Option<Arc<Data>> {
        self.data.read().unwrap().clone()
    }

    /// Load `places.sqlite` if it's appeared or changed since; returns whether it did.
    pub fn reload_if_changed(&self) -> bool {
        let current = self.data();
        let now = mtime(&self.layout.places());
        if now.is_none() || current.as_ref().is_some_and(|c| now == c.places_mtime) {
            return false;
        }
        match Data::load(&self.layout, self.offline) {
            Ok(d) => {
                d.gazetteer.warm_up();
                match current {
                    Some(_) => log::info!("loaded new place data ({})", d.data_version()),
                    None => log::info!("place data loaded ({}); answering lookups", d.data_version()),
                }
                *self.data.write().unwrap() = Some(Arc::new(d));
                true
            }
            Err(e) if current.is_some() => {
                log::warn!("couldn't load the new place data, keeping the old: {e:#}");
                false
            }
            Err(e) => {
                log::warn!("couldn't load the place data yet: {e:#}");
                false
            }
        }
    }

    /// Check for new place data in the background: every `every`, or every few seconds while
    /// there's none yet.
    pub fn watch_for_new_data(self: &Arc<Self>, every: Duration) {
        let me = self.clone();
        std::thread::spawn(move || {
            loop {
                let wait = if me.data().is_some() { every } else { WAITING_CHECK_EVERY };
                std::thread::sleep(wait);
                me.reload_if_changed();
            }
        });
    }

    fn lookup(&self, data: &Data, req: &PlaceRequest, caller: &Caller) -> Result<PlaceReply, Response> {
        let at = req.at();
        if !at.is_valid() {
            return Err(Response::error(400, "lat must be within ±90 and lon within ±180"));
        }
        let place = data.gazetteer.place(at).map_err(|e| {
            log::warn!("lookup at {} failed: {e:#}", at.cache_key());
            Response::error(500, &format!("lookup failed: {e:#}"))
        })?;
        Ok(PlaceReply {
            complete: place.as_ref().is_none_or(|p| p.complete),
            place,
            data_version: data.data_version(),
            rev: caller.rev,
        })
    }

    fn me(&self, caller: &Caller) -> Me {
        let data = self.data();
        Me {
            user: caller.user.clone(),
            device: caller.device.clone(),
            history: caller.history,
            max_wait_hours: caller.max_wait_hours,
            rev: caller.rev,
            data_version: data.as_ref().map_or_else(|| "loading".into(), |d| d.data_version()),
            server_version: photo_stamp_http::VERSION.into(),
            places_built: data.as_ref().and_then(|d| d.gazetteer.places_built()),
            overture_release: data.and_then(|d| d.gazetteer.overture_health()).and_then(|o| o.release),
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        if req.method == "GET" && req.path == "/healthz" {
            return match self.data() {
                Some(_) => Response::new(200, "text/plain", "ok"),
                None => Response::new(
                    503,
                    "text/plain",
                    "loading: waiting for the first import to build the place data",
                ),
            };
        }
        let caller = match bearer(&req).map(|k| self.store.authenticate(k)) {
            Some(Ok(Some(c))) => c,
            Some(Err(e)) => {
                log::warn!("checking a key failed: {e:#}");
                return Response::error(500, "couldn't check the key");
            }
            _ => return Response::error(401, "missing or unknown device key"),
        };
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/place") => {
                let Ok(body) = req.json::<PlaceRequest>() else {
                    return Response::error(400, "expected {\"lat\": …, \"lon\": …}");
                };
                let Some(data) = self.data() else { return still_loading() };
                match self.lookup(&data, &body, &caller) {
                    Ok(reply) => {
                        self.remember(&caller, &[body.at()]);
                        Response::json(200, &reply)
                    }
                    Err(r) => r,
                }
            }
            ("POST", "/v1/places") => {
                let Ok(body) = req.json::<BatchRequest>() else {
                    return Response::error(400, "expected {\"points\": [{\"lat\": …, \"lon\": …}, …]}");
                };
                if body.points.len() > MAX_BATCH {
                    return Response::error(400, &format!("at most {MAX_BATCH} points at a time"));
                }
                let Some(data) = self.data() else { return still_loading() };
                let mut replies = Vec::with_capacity(body.points.len());
                for p in &body.points {
                    match self.lookup(&data, p, &caller) {
                        Ok(r) => replies.push(r),
                        Err(r) => return r,
                    }
                }
                self.remember(&caller, &body.points.iter().map(PlaceRequest::at).collect::<Vec<_>>());
                Response::json(200, &BatchReply { replies })
            }
            ("GET", "/v1/me") => Response::json(200, &self.me(&caller)),
            ("PUT", "/v1/me") => {
                let Ok(update) = req.json::<MeUpdate>() else { return Response::error(400, "bad settings") };
                let result = (|| -> anyhow::Result<()> {
                    if let Some(h) = update.history {
                        self.store.set_history(&caller.device_id, h)?;
                    }
                    if let Some(w) = update.max_wait_hours {
                        self.store.set_max_wait(caller.user_id, w.min(24 * 365))?;
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    return Response::error(500, &format!("{e:#}"));
                }
                match bearer(&req).map(|k| self.store.authenticate(k)) {
                    Some(Ok(Some(c))) => Response::json(200, &self.me(&c)),
                    _ => Response::error(500, "couldn't read the settings back"),
                }
            }
            ("GET", "/v1/client/latest") => {
                let Some(u) = &self.updates else {
                    return Response::error(404, "updates aren't set up on this server");
                };
                let channel = req.param("channel").unwrap_or_else(|| "stable".into());
                match u.latest(&channel) {
                    Ok(Some(update)) => Response::json(200, &update),
                    Ok(None) => Response::new(204, "application/json", ""),
                    Err(e) => {
                        log::warn!("listing client updates: {e:#}");
                        Response::error(502, &format!("{e:#}"))
                    }
                }
            }
            ("GET", p) if p.starts_with("/v1/client/download/") => {
                let Some(u) = &self.updates else {
                    return Response::error(404, "updates aren't set up on this server");
                };
                let version = p.trim_start_matches("/v1/client/download/");
                match u.download(version) {
                    Ok((size, mut body)) => {
                        log::info!(
                            "{}'s {} is downloading photo-stamp {version}",
                            caller.user,
                            caller.device
                        );
                        let r = Response::stream(
                            "application/zip",
                            Box::new(move |out| photo_stamp_http::pump(&mut body, out)),
                        );
                        match size {
                            Some(n) => r.with_header("Content-Length", &n.to_string()),
                            None => r,
                        }
                    }
                    Err(e) => Response::error(404, &format!("{e:#}")),
                }
            }
            ("GET", "/v1/towns.sqlite") => match std::fs::read(self.layout.towns()) {
                Ok(bytes) => Response::new(200, "application/vnd.sqlite3", bytes),
                Err(_) if self.data().is_none() => still_loading(),
                Err(_) => Response::error(404, "no towns database yet; run photo-stamp-server import"),
            },
            (_, p) if p.starts_with("/v1/") => Response::error(404, "no such endpoint"),
            _ => Response::error(404, "not found"),
        }
    }

    fn remember(&self, caller: &Caller, points: &[GpsPoint]) {
        if let Err(e) = self.store.record(caller, points) {
            log::warn!("couldn't record history: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        let dir = std::env::temp_dir().join(format!(
            "ps-server-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/places.sqlite"),
            dir.join(FILE_NAME),
        )
        .unwrap();
        Layout { root: dir }
    }

    fn service() -> (Service, String) {
        let layout = layout();
        let store = Store::in_memory().unwrap();
        store.add_user("Dave").unwrap();
        let key = store.add_device("Dave", "laptop", true).unwrap();
        // Bundled data only (no Overture) keeps the tests offline.
        let data = Data::load(&layout, true).unwrap();
        (Service::new(store, Some(data), layout, true, None), key)
    }

    fn req(method: &str, path: &str, key: Option<&str>, body: &str) -> Request {
        let mut headers = Vec::new();
        if let Some(k) = key {
            headers.push(("authorization".to_owned(), format!("Bearer {k}")));
        }
        Request {
            method: method.into(),
            path: path.into(),
            headers,
            body: body.as_bytes().to_vec(),
            ..Request::default()
        }
    }

    fn json(r: Response) -> serde_json::Value {
        match r.body {
            photo_stamp_http::Body::Bytes(b) => serde_json::from_slice(&b).unwrap(),
            photo_stamp_http::Body::Stream(_) => panic!("streamed"),
        }
    }

    #[test]
    fn names_points_for_known_devices_only() {
        let (svc, key) = service();
        assert_eq!(svc.handle(req("GET", "/healthz", None, "")).status, 200);
        assert_eq!(svc.handle(req("POST", "/v1/place", None, r#"{"lat":48.8556,"lon":2.2986}"#)).status, 401);
        assert_eq!(svc.handle(req("POST", "/v1/place", Some("psk_00_00"), "{}")).status, 401);

        let r = svc.handle(req("POST", "/v1/place", Some(&key), r#"{"lat":48.8556,"lon":2.2986}"#));
        assert_eq!(r.status, 200);
        let v = json(r);
        assert!(v["place"]["context"].as_str().unwrap().contains("Paris"), "{v}");
        assert_eq!(v["complete"], true);
        assert!(v["data_version"].as_str().unwrap().starts_with("none/"));

        assert_eq!(svc.handle(req("POST", "/v1/place", Some(&key), r#"{"lat":95,"lon":0}"#)).status, 400);
        let caller = svc.store.authenticate(&key).unwrap().unwrap();
        assert_eq!(svc.store.point_count(caller.user_id).unwrap(), 1, "history is on for this device");
    }

    #[test]
    fn answers_while_loading_then_picks_the_data_up() {
        let layout = layout();
        let places = layout.places();
        let saved = places.with_extension("later");
        std::fs::rename(&places, &saved).unwrap();
        let store = Store::in_memory().unwrap();
        store.add_user("Dave").unwrap();
        let key = store.add_device("Dave", "laptop", true).unwrap();
        let svc = Service::new(store, None, layout.clone(), true, None);

        // Up, and honest about it.
        let r = svc.handle(req("GET", "/healthz", None, ""));
        assert_eq!(r.status, 503);
        let r = svc.handle(req("POST", "/v1/place", Some(&key), r#"{"lat":48.8556,"lon":2.2986}"#));
        assert_eq!(r.status, 503);
        assert!(r.headers.iter().any(|(n, v)| n == "Retry-After" && v == "60"));
        assert!(json(r)["error"].as_str().unwrap().contains("still loading"));
        assert_eq!(svc.handle(req("GET", "/v1/towns.sqlite", Some(&key), "")).status, 503);
        // Who you are still works (the dashboard can connect and say why).
        let me = json(svc.handle(req("GET", "/v1/me", Some(&key), "")));
        assert_eq!((me["user"].as_str(), me["data_version"].as_str()), (Some("Dave"), Some("loading")));
        assert_eq!(svc.handle(req("POST", "/v1/place", None, "{}")).status, 401, "keys are still checked");

        // The import finishes: picked up without a restart.
        assert!(!svc.reload_if_changed(), "nothing there yet");
        std::fs::rename(&saved, &places).unwrap();
        assert!(svc.reload_if_changed());
        assert_eq!(svc.handle(req("GET", "/healthz", None, "")).status, 200);
        let r = svc.handle(req("POST", "/v1/place", Some(&key), r#"{"lat":48.8556,"lon":2.2986}"#));
        assert_eq!(r.status, 200);
    }

    #[test]
    fn batches_and_settings() {
        let (svc, key) = service();
        let body = r#"{"points":[{"lat":48.8556,"lon":2.2986},{"lat":35.6586,"lon":139.7454}]}"#;
        let v = json(svc.handle(req("POST", "/v1/places", Some(&key), body)));
        assert_eq!(v["replies"].as_array().unwrap().len(), 2);
        let too_many = format!(r#"{{"points":[{}]}}"#, vec![r#"{"lat":1,"lon":1}"#; MAX_BATCH + 1].join(","));
        assert_eq!(svc.handle(req("POST", "/v1/places", Some(&key), &too_many)).status, 400);

        let me = json(svc.handle(req("GET", "/v1/me", Some(&key), "")));
        assert_eq!(
            (me["user"].as_str(), me["device"].as_str(), me["history"].as_bool()),
            (Some("Dave"), Some("laptop"), Some(true))
        );
        let me =
            json(svc.handle(req("PUT", "/v1/me", Some(&key), r#"{"history":false,"max_wait_hours":6}"#)));
        assert_eq!((me["history"].as_bool(), me["max_wait_hours"].as_u64()), (Some(false), Some(6)));
        assert_eq!(svc.handle(req("GET", "/v1/towns.sqlite", Some(&key), "")).status, 404, "not built yet");
    }
}
