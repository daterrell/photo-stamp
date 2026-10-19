//! Naming places by asking the family's place server (`photo-stamp-server`).
//!
//! - Answers are remembered on this PC ([`Answers`]), so repeat spots are instant and work offline.
//! - When the server can't be reached, lookups fail fast for a while (photos wait in the queue)
//!   rather than every photo waiting for a timeout.
//! - A small towns database (`towns.sqlite`, from the server, or the one bundled with the program)
//!   shows where waiting photos are from, and stamps them if they wait too long.
//!
//! The server's address and this device's key are kept in `client.json` in the data folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use photo_stamp_proto::api::{ClientUpdate, Me, MeUpdate, PlaceReply, PlaceRequest};
use photo_stamp_proto::places::{PlacesDb, TOWNS_FILE_NAME, locate};
use photo_stamp_proto::{GpsPoint, Place};
use serde::{Deserialize, Serialize};

use crate::answers::Answers;
use crate::geo::{GeoHealth, Geocoder, ServerHealth};

/// After a failed request, don't try the server again for this long.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);
/// Refresh who we are (and the server's data version) this often.
const ME_EVERY: Duration = Duration::from_secs(10 * 60);
/// Used until the server has told us the person's own wait time.
const DEFAULT_MAX_WAIT_HOURS: u32 = 72;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub url: String,
    pub key: String,
}

/// `client.json`: the place server for this PC, plus the last thing it said about us (so the wait
/// time is known even when starting offline).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub server: Option<ServerConfig>,
    #[serde(default)]
    pub last_me: Option<Me>,
    /// Which builds to update to: "stable" (releases) or "beta" (every build of main).
    #[serde(default)]
    pub channel: Option<String>,
}

impl ClientConfig {
    pub fn path(data: &Path) -> PathBuf {
        data.join("client.json")
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        // The device key is a secret: only this user may read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    }
}

/// `https://host` or `http://host:port`, without a trailing slash.
pub fn normalize_url(url: &str) -> anyhow::Result<String> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() < 10 {
        bail!("the place server's address should start with https:// (or http:// on a private network)");
    }
    Ok(url.to_owned())
}

#[derive(Default)]
struct Net {
    connected: Option<bool>,
    last_error: Option<String>,
    latency_ms: Option<u64>,
    /// Don't try the server again before this.
    retry_at: Option<Instant>,
    me_at: Option<Instant>,
}

pub struct RemoteGeocoder {
    data: Option<PathBuf>,
    config: RwLock<ClientConfig>,
    agent: ureq::Agent,
    answers: Option<Answers>,
    towns: RwLock<Option<PlacesDb>>,
    net: Mutex<Net>,
    hits: AtomicU64,
    asked: AtomicU64,
    /// Never ask the server (`--offline`): towns only.
    offline: bool,
    /// One towns download at a time.
    updating_towns: Mutex<()>,
}

impl RemoteGeocoder {
    /// `data`: photo-stamp's data folder (config, answers, downloaded towns); `None` keeps
    /// everything in memory.
    pub fn new(data: Option<&Path>, offline: bool) -> Arc<Self> {
        let mut config = data.map(|d| ClientConfig::load(&ClientConfig::path(d))).unwrap_or_default();
        // The Docker image is set up through its environment instead.
        if let (Ok(url), Ok(key)) =
            (std::env::var("PHOTO_STAMP_SERVER_URL"), std::env::var("PHOTO_STAMP_SERVER_KEY"))
        {
            match normalize_url(&url) {
                Ok(url) => config.server = Some(ServerConfig { url, key }),
                Err(e) => log::warn!("PHOTO_STAMP_SERVER_URL: {e:#}"),
            }
        }
        let answers = data.and_then(|d| match Answers::open(&d.join("answers.sqlite")) {
            Ok(a) => Some(a),
            Err(e) => {
                log::warn!("can't remember place answers: {e:#}");
                None
            }
        });
        let me = Arc::new(Self {
            data: data.map(Path::to_owned),
            config: RwLock::new(config),
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(5)))
                .timeout_global(Some(Duration::from_secs(30)))
                .user_agent(format!("photo-stamp/{}", photo_stamp_http::VERSION))
                .build()
                .into(),
            answers,
            towns: RwLock::new(None),
            net: Mutex::new(Net::default()),
            hits: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            offline,
            updating_towns: Mutex::new(()),
        });
        me.open_towns();
        me
    }

    fn towns_path(&self) -> Option<PathBuf> {
        let downloaded = self.data.as_ref().map(|d| d.join(TOWNS_FILE_NAME)).filter(|p| p.is_file());
        downloaded.or_else(|| locate(TOWNS_FILE_NAME, "PHOTO_STAMP_TOWNS_DB"))
    }

    /// The update channel: "stable" (the default) or "beta".
    pub fn channel(&self) -> String {
        self.config.read().unwrap().channel.clone().unwrap_or_else(|| "stable".into())
    }

    pub fn set_channel(&self, channel: &str) {
        self.config.write().unwrap().channel = Some(channel.to_owned());
        self.save_config();
    }

    /// The newest build on this device's channel, if the server offers updates.
    pub fn latest_update(&self) -> anyhow::Result<Option<ClientUpdate>> {
        let server = self.server().context("no place server set up")?;
        let bytes =
            self.call(&server, "GET", &format!("/v1/client/latest?channel={}", self.channel()), None)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Download `path` from the server into `dest`, reporting progress; returns its SHA-256 (hex).
    pub fn download(
        &self,
        path: &str,
        dest: &Path,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let server = self.server().context("no place server set up")?;
        // Big and slow is fine here; stalled isn't.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .user_agent(format!("photo-stamp/{}", photo_stamp_http::VERSION))
            .build()
            .into();
        let mut resp = agent
            .get(&format!("{}{path}", server.url))
            .header("Authorization", &format!("Bearer {}", server.key))
            .call()
            .with_context(|| format!("can't reach the place server at {}", server.url))?;
        anyhow::ensure!(
            resp.status() == 200,
            "the place server answered HTTP {} for the download",
            resp.status()
        );
        let total =
            resp.headers().get("content-length").and_then(|h| h.to_str().ok()).and_then(|s| s.parse().ok());
        let mut body = resp.body_mut().with_config().limit(1 << 30).reader();
        let mut out = std::fs::File::create(dest)?;
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            done += n as u64;
            progress(done, total);
        }
        out.flush()?;
        Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Let go of the towns database file (so an update can replace it); [`Self::open_towns`]
    /// opens it again.
    pub fn close_towns(&self) {
        *self.towns.write().unwrap() = None;
    }

    pub fn open_towns(&self) {
        let db = self.towns_path().and_then(|p| match PlacesDb::open(&p) {
            Ok(db) => Some(db),
            Err(e) => {
                log::warn!("can't open the towns database: {e:#}");
                None
            }
        });
        *self.towns.write().unwrap() = db;
    }

    pub fn server(&self) -> Option<ServerConfig> {
        if self.offline { None } else { self.config.read().unwrap().server.clone() }
    }

    fn save_config(&self) {
        let Some(d) = &self.data else { return };
        if let Err(e) = self.config.read().unwrap().save(&ClientConfig::path(d)) {
            log::warn!("couldn't save the place server settings: {e}");
        }
    }

    /// Use this place server from now on (`None`: no server, towns only).
    pub fn set_server(self: &Arc<Self>, server: Option<ServerConfig>) {
        {
            let mut c = self.config.write().unwrap();
            c.server = server;
            c.last_me = None;
        }
        *self.net.lock().unwrap() = Net::default();
        self.save_config();
        self.warm_up();
    }

    fn call(
        &self,
        server: &ServerConfig,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}{path}", server.url);
        let auth = format!("Bearer {}", server.key);
        let start = Instant::now();
        let result = match (method, body) {
            ("POST", Some(b)) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(b),
            ("PUT", Some(b)) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(b),
            _ => self.agent.get(&url).header("Authorization", &auth).call(),
        };
        // Whether a failure means lookups can't be answered now (unreachable, wrong key, loading
        // or broken), as opposed to the server answering "no" to this one request (e.g. 404 from a
        // server without client updates): only the former makes lookups wait.
        let mut unavailable = true;
        let outcome = (|| -> anyhow::Result<Vec<u8>> {
            let mut resp =
                result.with_context(|| format!("can't reach the place server at {}", server.url))?;
            let status = resp.status().as_u16();
            unavailable = status == 401 || status >= 500;
            let bytes = resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
            match status {
                200 | 204 => Ok(bytes),
                401 => bail!("the place server doesn't accept this device's key"),
                _ => {
                    let msg = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(|v| v["error"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| format!("HTTP {status}"));
                    bail!("the place server said: {msg}")
                }
            }
        })();
        let mut net = self.net.lock().unwrap();
        match &outcome {
            Ok(_) => {
                net.connected = Some(true);
                net.last_error = None;
                net.latency_ms = Some(start.elapsed().as_millis() as u64);
                net.retry_at = None;
            }
            Err(_) if !unavailable => {
                net.connected = Some(true);
                net.latency_ms = Some(start.elapsed().as_millis() as u64);
            }
            Err(e) => {
                if net.connected != Some(false) {
                    log::warn!("place server {}: {e:#}", server.url);
                }
                net.connected = Some(false);
                net.last_error = Some(format!("{e:#}"));
                net.retry_at = Some(Instant::now() + RETRY_AFTER_FAILURE);
            }
        }
        outcome
    }

    /// Ask the server who we are (and its data version); keeps the answer for when we're offline.
    pub fn refresh_me(&self) -> anyhow::Result<Me> {
        let server = self.server().context("no place server set up")?;
        let me: Me = serde_json::from_slice(&self.call(&server, "GET", "/v1/me", None)?)?;
        self.net.lock().unwrap().me_at = Some(Instant::now());
        let changed = self.config.read().unwrap().last_me.as_ref() != Some(&me);
        if changed {
            self.config.write().unwrap().last_me = Some(me.clone());
            self.save_config();
        }
        Ok(me)
    }

    /// Change the person's wait time on the server.
    pub fn update_me(&self, update: &MeUpdate) -> anyhow::Result<Me> {
        let server = self.server().context("no place server set up")?;
        let me: Me = serde_json::from_slice(&self.call(
            &server,
            "PUT",
            "/v1/me",
            Some(&serde_json::to_vec(update)?),
        )?)?;
        self.config.write().unwrap().last_me = Some(me.clone());
        self.save_config();
        Ok(me)
    }

    fn last_me(&self) -> Option<Me> {
        self.config.read().unwrap().last_me.clone()
    }

    /// Download the server's towns database if it's newer than ours.
    fn update_towns(&self, me: &Me) -> anyhow::Result<()> {
        let Some(data) = &self.data else { return Ok(()) };
        let _one = self.updating_towns.lock().unwrap();
        let ours = self.towns.read().unwrap().as_ref().and_then(PlacesDb::built);
        if me.places_built.is_none() || ours >= me.places_built {
            return Ok(());
        }
        let server = self.server().context("no place server set up")?;
        let bytes = self.call(&server, "GET", "/v1/towns.sqlite", None)?;
        let next = data.join("towns.sqlite.new");
        std::fs::write(&next, &bytes)?;
        PlacesDb::open(&next).context("the downloaded towns database doesn't open")?;
        // Windows can't replace a file that's open: close ours first.
        let mut towns = self.towns.write().unwrap();
        *towns = None;
        std::fs::rename(&next, data.join(TOWNS_FILE_NAME))?;
        *towns = PlacesDb::open(&data.join(TOWNS_FILE_NAME)).ok();
        log::info!(
            "updated the towns database ({:.1} MB) from the place server",
            bytes.len() as f64 / 1_048_576.0
        );
        Ok(())
    }

    fn town_place(&self, at: GpsPoint) -> Option<Place> {
        let towns = self.towns.read().unwrap();
        let db = towns.as_ref()?;
        let town = db.town(at).ok().flatten()?;
        // In a big city's districts, name the city (as the server does).
        let primary = db.big_city(at).ok().flatten().unwrap_or(town.town);
        // "Moscow, Russia", not "Moscow, Moscow, Russia" where the region is the city.
        let context: Vec<String> = [town.region, town.country]
            .into_iter()
            .flatten()
            .filter(|n| !n.preferred.eq_ignore_ascii_case(&primary.preferred))
            .map(|n| n.display())
            .collect();
        Some(Place {
            primary: primary.display(),
            near: None,
            context: (!context.is_empty()).then(|| context.join(", ")),
            complete: true,
        })
    }

    /// A cached answer still good for the server's current data.
    fn fresh(&self, at: GpsPoint) -> Option<Option<Place>> {
        let cached = self.answers.as_ref()?.get(at)?;
        let me = self.last_me();
        let current =
            me.as_ref().is_none_or(|m| m.data_version == cached.data_version && m.rev == cached.rev);
        current.then_some(cached.place)
    }
}

impl Geocoder for Arc<RemoteGeocoder> {
    fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>> {
        // No server: the towns database is all there is, so there's nothing to wait for.
        let Some(server) = self.server() else { return Ok(self.town_place(at)) };
        if let Some(hit) = self.fresh(at) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        // Copied out first: the lock mustn't be held while it's taken again below.
        let (retry_at, last_error) = {
            let net = self.net.lock().unwrap();
            (net.retry_at, net.last_error.clone())
        };
        if retry_at.is_some_and(|t| Instant::now() < t) {
            return Err(anyhow!("the place server can't be reached ({})", last_error.unwrap_or_default()));
        }
        // Refresh who we are now and then (cheap), so stale cached answers are noticed.
        if self.net.lock().unwrap().me_at.is_none_or(|t| t.elapsed() >= ME_EVERY) {
            let _ = self.refresh_me();
        }
        let body = serde_json::to_vec(&PlaceRequest::from(at))?;
        let reply: PlaceReply =
            serde_json::from_slice(&self.call(&server, "POST", "/v1/place", Some(&body))?)?;
        self.asked.fetch_add(1, Ordering::Relaxed);
        if reply.complete
            && let Some(a) = &self.answers
        {
            a.put(at, reply.place.as_ref(), &reply.data_version, reply.rev);
        }
        Ok(reply.into_place())
    }

    fn local_place(&self, at: GpsPoint) -> Option<Place> {
        // Any remembered answer beats the nearest town, even from older data.
        if let Some(cached) = self.answers.as_ref().and_then(|a| a.get(at)) {
            return cached.place;
        }
        self.town_place(at)
    }

    fn nearby_town(&self, at: GpsPoint) -> Option<String> {
        self.town_place(at).map(|p| p.primary)
    }

    fn max_wait(&self) -> Option<Duration> {
        self.server()?;
        let hours = self.last_me().map_or(DEFAULT_MAX_WAIT_HOURS, |m| m.max_wait_hours);
        Some(Duration::from_secs(u64::from(hours) * 3600))
    }

    fn warm_up(&self) {
        if self.server().is_none() {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || match me.refresh_me() {
            Ok(m) => {
                log::info!("place server: connected as {}'s {} (data {})", m.user, m.device, m.data_version);
                if let Err(e) = me.update_towns(&m) {
                    log::warn!("couldn't update the towns database: {e:#}");
                }
            }
            Err(e) => log::warn!("place server: {e:#}; photos will wait until it can be reached"),
        });
    }

    fn health(&self) -> GeoHealth {
        let towns_built = self.towns.read().unwrap().as_ref().and_then(PlacesDb::built);
        let server = self.server().map(|s| {
            let net = self.net.lock().unwrap();
            let me = self.last_me();
            ServerHealth {
                url: s.url,
                connected: net.connected,
                last_error: net.last_error.clone(),
                latency_ms: net.latency_ms,
                user: me.as_ref().map(|m| m.user.clone()),
                device: me.as_ref().map(|m| m.device.clone()),
                max_wait_hours: me.as_ref().map(|m| m.max_wait_hours),
                data_version: me.as_ref().map(|m| m.data_version.clone()),
                server_version: me.as_ref().map(|m| m.server_version.clone()),
                places_built: me.as_ref().and_then(|m| m.places_built.clone()),
                overture_release: me.as_ref().and_then(|m| m.overture_release.clone()),
                cached_answers: self.answers.as_ref().map_or(0, Answers::count),
                cache_hits: self.hits.load(Ordering::Relaxed),
                server_answers: self.asked.load(Ordering::Relaxed),
            }
        });
        GeoHealth { towns_built, server }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ps-remote-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in place server: names everything "Somewhere" and counts requests.
    fn fake_server(count: Arc<AtomicU64>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            photo_stamp_http::serve(
                listener,
                Arc::new(move |req: photo_stamp_http::Request| {
                    if req.header("authorization") != Some("Bearer psk_test_key") {
                        return photo_stamp_http::Response::error(401, "key");
                    }
                    match req.path.as_str() {
                        "/v1/me" => photo_stamp_http::Response::json(
                            200,
                            &Me {
                                user: "Dave".into(),
                                device: "laptop".into(),
                                history: true,
                                max_wait_hours: 5,
                                rev: 0,
                                data_version: "r1".into(),
                                server_version: "test".into(),
                                places_built: None,
                                overture_release: None,
                            },
                        ),
                        "/v1/place" => {
                            count.fetch_add(1, Ordering::Relaxed);
                            photo_stamp_http::Response::json(
                                200,
                                &PlaceReply {
                                    place: Some(Place { primary: "Somewhere".into(), ..Place::default() }),
                                    complete: true,
                                    data_version: "r1".into(),
                                    rev: 0,
                                },
                            )
                        }
                        _ => photo_stamp_http::Response::error(404, "no"),
                    }
                }),
            )
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn asks_once_per_spot_and_remembers() {
        let data = temp("ask");
        let count = Arc::new(AtomicU64::new(0));
        let url = fake_server(count.clone());
        let geo = RemoteGeocoder::new(Some(&data), false);
        geo.set_server(Some(ServerConfig { url: url.clone(), key: "psk_test_key".into() }));
        let at = GpsPoint { lat: 10.0, lon: 20.0 };
        assert_eq!(geo.place(at).unwrap().unwrap().primary, "Somewhere");
        assert_eq!(geo.place(at).unwrap().unwrap().primary, "Somewhere");
        assert_eq!(count.load(Ordering::Relaxed), 1, "the second was remembered");
        assert_eq!(geo.max_wait(), Some(Duration::from_secs(5 * 3600)), "the person's own wait time");

        // Remembered across restarts, and usable when the server is gone.
        let again = RemoteGeocoder::new(Some(&data), false);
        again.set_server(Some(ServerConfig { url: "http://127.0.0.1:9".into(), key: "psk_test_key".into() }));
        assert_eq!(again.local_place(at).unwrap().primary, "Somewhere");
        let err = again.place(GpsPoint { lat: 11.0, lon: 21.0 }).unwrap_err();
        assert!(format!("{err:#}").contains("place server"), "{err:#}");
        // Asking again soon after fails at once, without trying the network (or hanging).
        let start = Instant::now();
        assert!(again.place(GpsPoint { lat: 12.0, lon: 22.0 }).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(again.health().server.unwrap().connected, Some(false));
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn a_refused_request_doesnt_make_lookups_wait() {
        let data = temp("refused");
        let geo = RemoteGeocoder::new(Some(&data), false);
        geo.set_server(Some(ServerConfig {
            url: fake_server(Arc::new(AtomicU64::new(0))),
            key: "psk_test_key".into(),
        }));
        // This server doesn't offer client updates (404): it's still up.
        assert!(geo.latest_update().is_err());
        assert_eq!(geo.health().server.unwrap().connected, Some(true));
        assert_eq!(geo.place(GpsPoint { lat: 3.0, lon: 4.0 }).unwrap().unwrap().primary, "Somewhere");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn wrong_key_and_no_server() {
        let data = temp("key");
        let url = fake_server(Arc::new(AtomicU64::new(0)));
        let geo = RemoteGeocoder::new(Some(&data), false);
        geo.set_server(Some(ServerConfig { url, key: "psk_wrong".into() }));
        let err = geo.place(GpsPoint { lat: 1.0, lon: 1.0 }).unwrap_err();
        assert!(format!("{err:#}").contains("key"), "{err:#}");

        // Without a server there's nothing to wait for (and no towns database here either).
        let none = RemoteGeocoder::new(None, false);
        assert_eq!(none.place(GpsPoint { lat: 1.0, lon: 1.0 }).unwrap(), None);
        assert_eq!(none.max_wait(), None);
        assert!(normalize_url("ftp://x").is_err());
        assert_eq!(normalize_url(" https://places.example.com/ ").unwrap(), "https://places.example.com");
        std::fs::remove_dir_all(&data).unwrap();
    }
}
