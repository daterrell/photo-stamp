//! The dashboard and its API, served on http://127.0.0.1:8765 (see [`crate::dashboard`] for who
//! may use it).
//!
//! - `GET  /api/ping`: `{"app": "photo-stamp", "version", "pid"}`
//! - `GET  /api/app`: the drop folder, outbox, version and whether it starts at login
//! - `POST /api/startup` `{"on": bool}`: start at login or not
//! - `POST /api/open-folder` `{"which": "drop"|"outbox"}`: show it in Explorer
//! - `POST /api/restart`: stop and start again (e.g. after changing the outbox)
//! - `GET  /api/status`: everything at once ([`crate::status::Snapshot`])
//! - `GET  /api/events`: server-sent events as things happen
//! - `GET  /api/results?before=<id>&limit=<n>`: older results, newest first
//! - `GET  /api/thumb/<id>/<before|after>`: small previews of stamped photos
//! - `POST /api/restamp` `{"id": n, "lines": ["…"]}`: stamp a listed photo again with these caption
//!   lines (from its kept original), replacing its stamped copy; replies with its result
//! - `POST /api/history/clear`: forget the results and previews (all-time totals stay)
//! - `POST /api/upload?path=<path within the watched folder>`: a photo dropped on the dashboard,
//!   as the request body; it's put in the watched folder (a free name if that one's taken)
//! - `POST /api/pause`, `/api/resume`, `/api/stop` (quit)
//! - `POST /api/retry` `{"path": "<path within the watched folder>"}`
//! - `POST /api/stamp-now` `{"path": "…"}` or `{}` for every waiting photo: stop waiting for the
//!   place server and stamp with what's known on this PC
//! - `GET|PUT /api/settings`: PUT replies `{"settings": …, "restart": bool}`; outbox, originals and
//!   offline changes only apply after a restart.
//! - `GET /api/server`, `PUT /api/server` `{"url", "key"}` (an empty key keeps the current one),
//!   `DELETE /api/server`: the place server this PC uses; `POST /api/server/test`
//! - `PUT /api/me` `{"max_wait_hours": n}`: how long photos wait for the place server
//! - `POST /api/update/check`; `POST /api/update/install` (installs the downloaded update: finishes
//!   the photo in hand, then restarts as the new version). Updates come from GitHub releases; the
//!   updater's state is in `/api/status` (`update`).

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use photo_stamp_http::{Request, Response, sse_event};
use serde::Deserialize;
use serde_json::json;

use crate::outbox::Wake;
use crate::remote::{RemoteGeocoder, ServerConfig, normalize_url};
use crate::settings::Settings;
use crate::status::{Status, Which};

/// How often an idle event stream sends a comment, so dead connections are noticed.
const KEEPALIVE: Duration = Duration::from_secs(15);

pub struct Api {
    pub status: Arc<Status>,
    pub waker: Mutex<Sender<Wake>>,
    /// Where settings changes are saved (`None`: not saved).
    pub settings_path: Option<PathBuf>,
    /// The place server connection (`None` in tests).
    pub geo: Option<Arc<RemoteGeocoder>>,
    pub updater: Option<Arc<crate::update::Updater>>,
    /// The drop folder and where stamped photos go.
    pub root: PathBuf,
    pub outbox: PathBuf,
    /// Where the dashboard is served (set once listening): requests must be addressed to it.
    pub port: AtomicU16,
    /// The start-at-login entry, where that's supported (Windows).
    pub startup: Option<Startup>,
    /// Asked to restart (stop, then start again).
    pub restart: AtomicBool,
    /// Shutting down: open event streams end.
    pub closing: Arc<AtomicBool>,
    /// Where the originals of stamped photos are kept (`None`: `originals` in the watched folder).
    pub originals: Option<PathBuf>,
    /// For stamping photos again from the dashboard (made when first needed: loading fonts is slow).
    renderer: Mutex<Option<crate::render::Renderer>>,
}

/// photo-stamp's start-at-login entry: its name and the command it runs.
pub struct Startup {
    pub name: String,
    pub command: String,
}

impl Startup {
    pub fn is_on(&self) -> bool {
        #[cfg(windows)]
        return crate::winsys::starts_at_login(&self.name);
        #[cfg(not(windows))]
        false
    }

    pub fn set(&self, on: bool) -> std::io::Result<()> {
        #[cfg(windows)]
        return if on {
            crate::winsys::install_startup(&self.name, &self.command)
        } else {
            crate::winsys::uninstall_startup(&self.name).map(drop)
        };
        #[cfg(not(windows))]
        {
            let _ = on;
            Err(std::io::Error::other("starting at login is a Windows feature"))
        }
    }
}

#[derive(Deserialize)]
struct StartupBody {
    on: bool,
}

#[derive(Deserialize)]
struct OpenBody {
    #[serde(default)]
    which: Option<String>,
}

#[derive(Deserialize)]
struct WaitBody {
    max_wait_hours: u32,
}

#[derive(Deserialize)]
struct StampNowBody {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct ServerBody {
    url: String,
    #[serde(default)]
    key: String,
}

/// The place server settings as the dashboard sees them (never the key itself).
fn server_view(geo: &RemoteGeocoder) -> serde_json::Value {
    let server = geo.server();
    json!({
        "url": server.as_ref().map(|s| s.url.clone()),
        "has_key": server.as_ref().is_some_and(|s| !s.key.is_empty()),
        "key_hint": server.as_ref().and_then(|s| s.key.split('_').nth(1).map(str::to_owned)),
    })
}

impl Api {
    pub fn new(status: Arc<Status>, waker: Sender<Wake>, root: &Path, outbox: &Path) -> Self {
        Self {
            status,
            waker: Mutex::new(waker),
            settings_path: None,
            geo: None,
            updater: None,
            root: root.to_owned(),
            outbox: outbox.to_owned(),
            port: AtomicU16::new(crate::dashboard::DEFAULT_PORT),
            startup: None,
            restart: AtomicBool::new(false),
            closing: Arc::new(AtomicBool::new(false)),
            originals: None,
            renderer: Mutex::new(None),
        }
    }

    fn wake(&self, w: Wake) {
        let _ = self.waker.lock().unwrap().send(w);
    }

    /// Pause or resume (from the dashboard or the tray icon), and remember it.
    pub fn set_paused(&self, paused: bool, from: &str) {
        if self.status.is_paused() != paused {
            log::info!("{} from {from}", if paused { "paused" } else { "resumed" });
        }
        self.update_settings(|s| s.paused = paused);
        if !paused {
            self.wake(Wake::Fs);
        }
    }

    /// Stop, then start again (as the same version).
    pub fn request_restart(&self) {
        log::info!("restarting");
        self.restart.store(true, Ordering::Relaxed);
        self.wake(Wake::Stop);
    }

    /// Quit (from the dashboard or the tray icon).
    pub fn quit(&self, from: &str) {
        log::info!("quit from {from}");
        self.wake(Wake::Stop);
    }

    /// Change the settings, apply them and save them.
    fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let mut s = self.status.settings();
        let was_paused = s.paused;
        change(&mut s);
        self.status.set_settings(s.clone());
        if s.paused != was_paused {
            self.status.set_paused(s.paused);
        }
        if let Some(path) = &self.settings_path
            && let Err(e) = s.save(path)
        {
            log::warn!("couldn't save settings to {}: {e}", path.display());
        }
    }
}

#[derive(Deserialize)]
struct RetryBody {
    path: String,
}

#[derive(Deserialize)]
struct RestampBody {
    id: u64,
    lines: Vec<String>,
}

/// Caption lines as typed: trimmed, blank ones dropped. At least one, at most six, each of
/// reasonable length.
fn caption_from(lines: Vec<String>) -> Result<Vec<String>, &'static str> {
    let lines: Vec<String> =
        lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    if lines.is_empty() {
        return Err("write at least one line");
    }
    if lines.len() > 6 {
        return Err("a stamp has at most six lines");
    }
    if lines.iter().any(|l| l.chars().count() > 200) {
        return Err("a line can be at most 200 characters");
    }
    Ok(lines)
}

impl Api {
    fn originals(&self) -> PathBuf {
        self.originals.clone().unwrap_or_else(|| self.root.join(crate::outbox::ORIGINALS_DIR))
    }

    /// Stamp a listed photo again with the person's caption.
    fn restamp(&self, body: RestampBody) -> Response {
        let lines = match caption_from(body.lines) {
            Ok(l) => l,
            Err(why) => return Response::error(400, why),
        };
        let Some(entry) = self.status.entry(body.id) else {
            return Response::error(404, "that photo is no longer listed");
        };
        let mut renderer = self.renderer.lock().unwrap();
        let renderer = renderer.get_or_insert_with(crate::render::Renderer::new);
        match crate::outbox::restamp(&self.status, &entry, lines, &self.originals(), renderer) {
            Ok(new) => {
                self.status.amend(new.clone(), entry.outcome);
                Response::json(200, &new)
            }
            Err(e) => Response::error(409, &format!("Couldn't stamp {} again: {e:#}", entry.path)),
        }
    }

    /// A photo dropped on the dashboard: put it in the watched folder for the watcher to stamp.
    fn upload(&self, mut req: Request) -> Response {
        let Some(rel) = req.param("path").as_deref().and_then(safe_relative) else {
            return Response::error(400, "expected ?path=<path within the watched folder>");
        };
        if crate::formats::classify(&rel).is_none() {
            return Response::error(415, "photo-stamp doesn't stamp this kind of file");
        }
        let dir = self.root.join(rel.parent().unwrap_or(Path::new("")));
        // As folders, case-insensitively: "outbox2" isn't inside "outbox".
        let lower = |p: &Path| {
            format!("{}/", p.to_string_lossy().to_lowercase().replace('\\', "/").trim_end_matches('/'))
        };
        let support = self.root.join(crate::outbox::SUPPORT_DIR);
        if [&self.outbox, &self.originals(), &support].iter().any(|d| lower(&dir).starts_with(&lower(d))) {
            return Response::error(400, "photos can't be added to the outbox or originals folder");
        }
        let Some(name) = rel.file_name() else { return Response::error(400, "no file name") };
        let expected = req.content_length();
        let result = (|| -> anyhow::Result<PathBuf> {
            std::fs::create_dir_all(&dir)?;
            // Hidden and ".part": the watcher leaves it alone until it's complete and renamed.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tmp = dir.join(format!(
                ".photo-stamp-upload-{}-{}.part",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let copied = (|| -> anyhow::Result<u64> {
                let mut out = std::fs::File::create(&tmp)?;
                let n = match req.stream.as_mut() {
                    Some(stream) => std::io::copy(stream, &mut out)?,
                    None => {
                        out.write_all(&req.body)?;
                        req.body.len() as u64
                    }
                };
                out.sync_all()?;
                Ok(n)
            })();
            match copied {
                Ok(n) if n == expected => {}
                Ok(_) => {
                    let _ = std::fs::remove_file(&tmp);
                    anyhow::bail!("the upload was cut short");
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
            let dest = crate::outbox::free_name(&dir, name, None);
            std::fs::rename(&tmp, &dest)?;
            Ok(dest)
        })();
        match result {
            Ok(dest) => {
                let shown = dest.strip_prefix(&self.root).unwrap_or(&dest).to_string_lossy().into_owned();
                log::info!("added       {shown} (dropped on the dashboard)");
                self.wake(Wake::Fs);
                Response::json(200, &json!({ "path": shown }))
            }
            Err(e) => Response::error(500, &format!("couldn't add {}: {e:#}", rel.display())),
        }
    }
}

/// A path within the watched folder, without any way out of it.
fn safe_relative(p: &str) -> Option<PathBuf> {
    let path = Path::new(p);
    path.components().all(|c| matches!(c, Component::Normal(_))).then(|| path.to_owned())
}

pub fn handle(api: &Api, req: Request) -> Response {
    if let Err(why) = crate::dashboard::allowed(api.port.load(Ordering::Relaxed), &req) {
        return Response::error(403, why);
    }
    let wake = |w: Wake| api.wake(w);
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET", _) if !req.path.starts_with("/api/") => crate::dashboard::asset(&req.path),
        ("GET", ["api", "ping"]) => Response::json(
            200,
            &json!({ "app": "photo-stamp", "version": photo_stamp_http::VERSION, "pid": std::process::id() }),
        ),
        ("GET", ["api", "app"]) => Response::json(
            200,
            &json!({
                "root": api.root,
                "outbox": api.outbox,
                "version": photo_stamp_http::VERSION,
                "pid": std::process::id(),
                "startup": api.startup.as_ref().map(Startup::is_on),
            }),
        ),
        ("POST", ["api", "startup"]) => {
            let Some(startup) = &api.startup else { return Response::error(404, "not on this system") };
            let Ok(body) = req.json::<StartupBody>() else {
                return Response::error(400, "expected {\"on\": true|false}");
            };
            match startup.set(body.on) {
                Ok(()) => {
                    log::info!("start at login: {}", if body.on { "on" } else { "off" });
                    Response::json(200, &json!({ "on": startup.is_on() }))
                }
                Err(e) => Response::error(500, &format!("couldn't change start at login: {e}")),
            }
        }
        ("POST", ["api", "open-folder"]) => {
            let body = req.json::<OpenBody>().unwrap_or(OpenBody { which: None });
            let dir = match body.which.as_deref() {
                Some("outbox") => &api.outbox,
                _ => &api.root,
            };
            crate::desktop::open(&dir.to_string_lossy());
            Response::ok()
        }
        ("POST", ["api", "restart"]) => {
            api.request_restart();
            Response::ok()
        }
        ("GET", ["api", "status"]) => api.status.snapshot(|s| Response::json(200, s)),
        ("GET", ["api", "events"]) => events(api),
        ("GET", ["api", "results"]) => {
            let before = req.param("before").and_then(|b| b.parse().ok());
            let limit = req.param("limit").and_then(|l| l.parse().ok()).unwrap_or(100).min(500);
            Response::json(200, &api.status.history(before, limit))
        }
        ("GET", ["api", "thumb", id, which]) => {
            let (Ok(id), Some(which)) = (id.parse::<u64>(), Which::parse(which)) else {
                return Response::error(404, "no such preview");
            };
            match api.status.thumb_path(id, which).and_then(|p| std::fs::read(p).ok()) {
                Some(jpeg) => {
                    Response::new(200, "image/jpeg", jpeg).with_header("Cache-Control", "max-age=86400")
                }
                None => Response::error(404, "no such preview"),
            }
        }
        ("POST", ["api", "restamp"]) => match req.json::<RestampBody>() {
            Ok(body) => api.restamp(body),
            Err(_) => Response::error(400, "expected {\"id\": n, \"lines\": [\"…\"]}"),
        },
        ("POST", ["api", "history", "clear"]) => {
            log::info!("history cleared from the dashboard");
            api.status.clear_history();
            Response::ok()
        }
        ("POST", ["api", "upload"]) => api.upload(req),
        ("POST", ["api", "pause"]) => {
            api.set_paused(true, "the dashboard");
            Response::ok()
        }
        ("POST", ["api", "resume"]) => {
            api.set_paused(false, "the dashboard");
            Response::ok()
        }
        ("POST", ["api", "stop"]) => {
            api.quit("the dashboard");
            Response::ok()
        }
        ("POST", ["api", "retry"]) => match req.json::<RetryBody>().ok().and_then(|b| safe_relative(&b.path))
        {
            Some(rel) => {
                wake(Wake::Retry(rel));
                Response::ok()
            }
            None => Response::error(400, "expected {\"path\": \"<path within the watched folder>\"}"),
        },
        ("POST", ["api", "stamp-now"]) => {
            let body = req.json::<StampNowBody>().unwrap_or(StampNowBody { path: None });
            match body.path.as_deref().map(safe_relative) {
                Some(None) => {
                    Response::error(400, "expected {\"path\": \"<path within the watched folder>\"} or {}")
                }
                Some(Some(rel)) => {
                    wake(Wake::StampNow(Some(rel)));
                    Response::ok()
                }
                None => {
                    wake(Wake::StampNow(None));
                    Response::ok()
                }
            }
        }
        ("GET", ["api", "server"]) => match &api.geo {
            Some(g) => Response::json(200, &server_view(g)),
            None => Response::error(404, "no place lookups here"),
        },
        ("PUT", ["api", "server"]) => {
            let Some(geo) = &api.geo else { return Response::error(404, "no place lookups here") };
            let Ok(body) = req.json::<ServerBody>() else {
                return Response::error(400, "expected {\"url\": \"https://…\", \"key\": \"psk_…\"}");
            };
            let url = match normalize_url(&body.url) {
                Ok(u) => u,
                Err(e) => return Response::error(400, &format!("{e:#}")),
            };
            let key = match body.key.trim() {
                "" => geo.server().map(|s| s.key).unwrap_or_default(),
                k => k.to_owned(),
            };
            if !key.starts_with("psk_") {
                return Response::error(
                    400,
                    "the device key starts with psk_ (from photo-stamp-server device add)",
                );
            }
            log::info!("place server set to {url}");
            geo.set_server(Some(ServerConfig { url, key }));
            let test = geo.refresh_me();
            let mut v = server_view(geo);
            v["test"] = match test {
                Ok(me) => json!({ "ok": true, "user": me.user, "device": me.device }),
                Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
            };
            wake(Wake::Fs);
            Response::json(200, &v)
        }
        ("DELETE", ["api", "server"]) => {
            let Some(geo) = &api.geo else { return Response::error(404, "no place lookups here") };
            log::info!("place server removed: naming places from this PC's towns database only");
            geo.set_server(None);
            Response::json(200, &server_view(geo))
        }
        ("POST", ["api", "server", "test"]) => {
            let Some(geo) = &api.geo else { return Response::error(404, "no place lookups here") };
            match geo.refresh_me() {
                Ok(me) => {
                    wake(Wake::Fs);
                    Response::json(200, &json!({ "ok": true, "user": me.user, "device": me.device }))
                }
                Err(e) => Response::json(200, &json!({ "ok": false, "error": format!("{e:#}") })),
            }
        }
        ("PUT", ["api", "me"]) => {
            let Some(geo) = &api.geo else { return Response::error(404, "no place lookups here") };
            let Ok(body) = req.json::<WaitBody>() else {
                return Response::error(400, "expected {\"max_wait_hours\": n}");
            };
            let update =
                photo_stamp_proto::api::MeUpdate { max_wait_hours: Some(body.max_wait_hours), history: None };
            match geo.update_me(&update) {
                Ok(me) => Response::json(200, &me),
                Err(e) => Response::error(502, &format!("{e:#}")),
            }
        }
        ("POST", ["api", "update", "check"]) => {
            let Some(u) = api.updater.clone() else { return Response::error(404, "no updates here") };
            std::thread::spawn(move || u.check());
            Response::ok()
        }
        ("POST", ["api", "update", "install"]) => {
            let Some(u) = &api.updater else { return Response::error(404, "no updates here") };
            match u.request_install() {
                Ok(version) => {
                    // The watcher finishes the photo in hand, stops, and main installs.
                    wake(Wake::Stop);
                    Response::json(200, &json!({ "version": version }))
                }
                Err(e) => Response::error(409, &format!("{e:#}")),
            }
        }
        ("GET", ["api", "settings"]) => Response::json(200, &api.status.settings()),
        ("PUT", ["api", "settings"]) => match req.json::<Settings>() {
            Ok(new) => {
                let new = new.sanitized();
                let restart = api.status.settings().needs_restart(&new);
                api.update_settings(|s| *s = new.clone());
                wake(Wake::Fs);
                Response::json(200, &json!({ "settings": new, "restart": restart }))
            }
            Err(e) => Response::error(400, &format!("bad settings: {e}")),
        },
        (_, ["api", ..]) => Response::error(404, "no such endpoint"),
        _ => Response::error(404, "not found"),
    }
}

/// Server-sent events until the page goes away or photo-stamp shuts down (which must close every
/// connection, so the port is free for the next copy after an update or restart).
fn events(api: &Api) -> Response {
    let status = api.status.clone();
    let closing = api.closing.clone();
    let rx = status.subscribe();
    Response::stream(
        "text/event-stream",
        Box::new(move |out: &mut dyn Write| {
            // Tell the client which event the stream starts after, so it can line up a snapshot.
            let seq = status.snapshot(|s| s.seq);
            out.write_all(sse_event(&json!({ "type": "hello", "seq": seq }).to_string()).as_bytes())?;
            out.flush()?;
            let mut quiet_since = Instant::now();
            while !closing.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(msg) => {
                        out.write_all(sse_event(&msg).as_bytes())?;
                        quiet_since = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) if quiet_since.elapsed() >= KEEPALIVE => {
                        out.write_all(b": keepalive\n\n")?;
                        quiet_since = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                out.flush()?;
            }
            Ok(())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};

    fn api() -> (Api, Receiver<Wake>) {
        let (tx, rx) = channel();
        let status = Arc::new(Status::new(
            Path::new("/drop"),
            Path::new("/drop/outbox"),
            None,
            "k",
            Settings::default(),
        ));
        (Api::new(status, tx, Path::new("/drop"), Path::new("/drop/outbox")), rx)
    }

    /// A request from the dashboard page on the default port.
    fn req(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: vec![
                ("host".into(), "127.0.0.1:8765".into()),
                ("origin".into(), "http://127.0.0.1:8765".into()),
            ],
            body: body.as_bytes().to_vec(),
            ..Request::default()
        }
    }

    fn json_of(r: Response) -> serde_json::Value {
        match r.body {
            photo_stamp_http::Body::Bytes(b) => serde_json::from_slice(&b).unwrap(),
            photo_stamp_http::Body::Stream(_) => panic!("streamed"),
        }
    }

    #[test]
    fn only_from_this_pc_and_the_dashboard_page() {
        let (api, _rx) = api();
        let mut r = req("GET", "/api/status", "");
        r.headers = vec![("host".into(), "evil.example:8765".into())];
        assert_eq!(handle(&api, r).status, 403);
        let mut r = req("POST", "/api/pause", "");
        r.headers.retain(|(n, _)| n != "origin");
        assert_eq!(handle(&api, r).status, 403, "changes need the page's Origin");
        assert!(!api.status.is_paused());
        assert_eq!(handle(&api, req("GET", "/api/status", "")).status, 200);
        assert_eq!(handle(&api, req("GET", "/", "")).status, 200, "the page itself");
    }

    #[test]
    fn ping_and_app() {
        let (api, _rx) = api();
        let v = json_of(handle(&api, req("GET", "/api/ping", "")));
        assert_eq!(
            (v["app"].as_str(), v["pid"].as_u64()),
            (Some("photo-stamp"), Some(u64::from(std::process::id())))
        );
        let v = json_of(handle(&api, req("GET", "/api/app", "")));
        assert_eq!(v["root"], "/drop");
        assert_eq!(v["startup"], serde_json::Value::Null, "no start-at-login entry here");
        assert_eq!(handle(&api, req("POST", "/api/startup", r#"{"on": true}"#)).status, 404);
    }

    #[test]
    fn written_captions_are_tidied_and_checked() {
        let lines = |v: &[&str]| caption_from(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(lines(&["  Home ", "", "2024"]).unwrap(), ["Home", "2024"]);
        assert!(lines(&["", "  "]).is_err());
        assert!(lines(&["a"; 7]).is_err());
        assert!(lines(&[&"x".repeat(201)]).is_err());
    }

    #[test]
    fn photos_dropped_on_the_page_go_into_the_watched_folder() {
        let root = crate::stamp::tests::temp_dir("api-upload");
        let (tx, rx) = channel();
        let status = Arc::new(Status::new(&root, &root.join("outbox"), None, "k", Settings::default()));
        let api = Api::new(status, tx, &root, &root.join("outbox"));
        let upload = |path: &str, body: &str| {
            let mut r = req("POST", "/api/upload", body);
            r.query = format!("path={path}");
            r.headers.push(("content-length".into(), body.len().to_string()));
            handle(&api, r)
        };
        let r = upload("trip%2Fday%201%2Fa.jpg", "jpeg bytes");
        assert_eq!(r.status, 200);
        assert_eq!(std::fs::read_to_string(root.join("trip/day 1/a.jpg")).unwrap(), "jpeg bytes");
        assert!(matches!(rx.try_recv(), Ok(Wake::Fs)), "the watcher is told");
        assert_eq!(upload("trip%2Fday%201%2Fa.jpg", "more").status, 200);
        assert_eq!(
            std::fs::read_to_string(root.join("trip/day 1/a (2).jpg")).unwrap(),
            "more",
            "never replaced"
        );
        assert_eq!(upload("notes.txt", "hi").status, 415);
        assert_eq!(upload("outbox%2Fa.jpg", "x").status, 400);
        assert_eq!(upload("..%2Fescape.jpg", "x").status, 400);
        assert!(!root.join("outbox/a.jpg").exists() && !root.join("notes.txt").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restart_stops_and_says_so() {
        let (api, rx) = api();
        assert_eq!(handle(&api, req("POST", "/api/restart", "")).status, 200);
        assert!(api.restart.load(Ordering::Relaxed));
        assert!(matches!(rx.try_recv(), Ok(Wake::Stop)));
    }

    #[test]
    fn status_snapshot_has_the_dashboard_fields() {
        let (api, _rx) = api();
        let v = json_of(handle(&api, req("GET", "/api/status", "")));
        for field in [
            "seq", "root", "outbox", "paused", "queue", "progress", "session", "recent", "health", "settings",
        ] {
            assert!(v.get(field).is_some(), "missing {field}: {v}");
        }
    }

    #[test]
    fn pause_resume_retry_and_stop_reach_the_watcher() {
        let (api, rx) = api();
        assert_eq!(handle(&api, req("POST", "/api/pause", "")).status, 200);
        assert!(api.status.is_paused());
        assert_eq!(handle(&api, req("POST", "/api/resume", "")).status, 200);
        assert!(!api.status.is_paused());
        assert!(matches!(rx.try_recv(), Ok(Wake::Fs)));

        assert_eq!(handle(&api, req("POST", "/api/retry", r#"{"path": "trip/a.jpg"}"#)).status, 200);
        assert!(matches!(rx.try_recv(), Ok(Wake::Retry(p)) if p == Path::new("trip/a.jpg")));
        for bad in [r#"{"path": "../x.jpg"}"#, r#"{"path": "/etc/passwd"}"#, "{}"] {
            assert_eq!(handle(&api, req("POST", "/api/retry", bad)).status, 400, "{bad}");
        }

        assert_eq!(handle(&api, req("POST", "/api/stop", "")).status, 200);
        assert!(matches!(rx.try_recv(), Ok(Wake::Stop)));
    }

    #[test]
    fn settings_changes_say_when_a_restart_is_needed() {
        let (api, _rx) = api();
        let mut s = Settings { quality: 80, ..Settings::default() };
        let v = json_of(handle(&api, req("PUT", "/api/settings", &serde_json::to_string(&s).unwrap())));
        assert_eq!(v["restart"], false);
        assert_eq!(api.status.settings().quality, 80);
        s.offline = true;
        let v = json_of(handle(&api, req("PUT", "/api/settings", &serde_json::to_string(&s).unwrap())));
        assert_eq!(v["restart"], true);
        assert_eq!(handle(&api, req("PUT", "/api/settings", "nope")).status, 400);
    }

    #[test]
    fn events_stream_over_http() {
        let (api, _rx) = api();
        let api = Arc::new(api);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        api.port.store(port, Ordering::Relaxed);
        let a = api.clone();
        let _server =
            photo_stamp_http::Server::spawn(listener, Arc::new(move |req| handle(&a, req))).unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = String::new();
        let mut buf = [0u8; 4096];
        let read_until = |got: &mut String, s: &mut std::net::TcpStream, buf: &mut [u8], what: &str| {
            while !got.contains(what) {
                let n = std::io::Read::read(s, buf).unwrap();
                assert!(n > 0, "stream ended: {got}");
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        };
        read_until(&mut got, &mut s, &mut buf, "\"hello\"");
        api.status.set_paused(true);
        read_until(&mut got, &mut s, &mut buf, "\"paused\"");
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(got.contains("Content-Type: text/event-stream"));
        // Shutting down ends the stream (so the port can be let go).
        api.closing.store(true, Ordering::Relaxed);
        let mut rest = Vec::new();
        std::io::Read::read_to_end(&mut s, &mut rest).unwrap();
    }
}
