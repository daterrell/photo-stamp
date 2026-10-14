//! The dashboard server: the page, the list of photo-stamp services, and a proxy to each one's
//! local API (adding its token, so the page never sees it).
//!
//! - `GET  /api/services`: every registered folder, whether it's running, and whether the
//!   photo-stamp next to this program could watch this program's own folder
//! - `POST /api/start-here`: start that photo-stamp, watching this program's folder
//! - `POST /api/s/<key>/start`, `/restart`: start (or stop and start) the service for a folder
//! - `GET|POST /api/s/<key>/startup`: whether it starts at login; POST `{"on": bool}` changes it
//! - `* /api/s/<key>/<rest>`: passed on to that service as `/api/<rest>` (status, events, …)
//!
//! Only this PC's browser can use it: requests must be addressed to 127.0.0.1/localhost (which
//! stops other web sites reaching it through DNS tricks), and anything that changes something
//! must come from the dashboard page itself (its `Origin`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use photo_stamp_http::registry::Registration;
use photo_stamp_http::{Request, Response, pump};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::system;

/// How long to wait for a service to come up or go down.
const START_WAIT: Duration = Duration::from_secs(20);
const STOP_WAIT: Duration = Duration::from_secs(60);

struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset { path: "/", content_type: "text/html; charset=utf-8", body: include_bytes!("../web/index.html") },
    Asset {
        path: "/app.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/app.css"),
    },
    Asset {
        path: "/app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/app.js"),
    },
    Asset { path: "/icon.svg", content_type: "image/svg+xml", body: include_bytes!("../web/icon.svg") },
    Asset {
        path: "/fonts/bricolage-latin.woff2",
        content_type: "font/woff2",
        body: include_bytes!("../web/fonts/bricolage-latin.woff2"),
    },
    Asset {
        path: "/fonts/bricolage-latin-ext.woff2",
        content_type: "font/woff2",
        body: include_bytes!("../web/fonts/bricolage-latin-ext.woff2"),
    },
];

pub struct Host {
    data: PathBuf,
    /// The folder this program is in: photo-stamp is usually beside it, watching it.
    exe_dir: Option<PathBuf>,
    port: u16,
    agent: ureq::Agent,
    /// For event streams, which stay open.
    stream_agent: ureq::Agent,
    last_activity: Mutex<Instant>,
    open_streams: Arc<AtomicUsize>,
    quit: AtomicBool,
    /// One start/stop at a time.
    managing: Mutex<()>,
}

/// One folder's service, as the page sees it.
#[derive(Debug, Serialize)]
struct Service {
    key: String,
    root: PathBuf,
    outbox: PathBuf,
    version: String,
    pid: u32,
    started: u64,
    /// Answering its API right now.
    running: bool,
    /// Whether it starts at login (`None`: not supported here).
    startup: Option<bool>,
    /// Whether we know which program to start for it.
    can_start: bool,
}

#[derive(Deserialize)]
struct StartupBody {
    on: bool,
}

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(timeout)
        .build()
        .into()
}

/// The photo-stamp program in `dir`.
fn service_exe(dir: &Path) -> PathBuf {
    if let Some(exe) = std::env::var_os("PHOTO_STAMP_EXE") {
        return PathBuf::from(exe);
    }
    dir.join(if cfg!(windows) { "photo-stamp.exe" } else { "photo-stamp" })
}

/// Whether a dashboard is already serving on `port`.
pub fn is_dashboard(port: u16) -> bool {
    let Ok(mut resp) =
        agent(Some(Duration::from_secs(2))).get(&format!("http://127.0.0.1:{port}/api/ping")).call()
    else {
        return false;
    };
    resp.body_mut()
        .read_to_string()
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["app"] == "photo-stamp-ui")
}

/// Ask the dashboard on `port` to exit. Returns whether one was running.
pub fn ask_to_quit(port: u16) -> bool {
    agent(Some(Duration::from_secs(5)))
        .post(&format!("http://127.0.0.1:{port}/api/quit"))
        .header("Origin", &format!("http://127.0.0.1:{port}"))
        .send_empty()
        .is_ok_and(|r| r.status() == 200)
}

/// Counts an open event stream for as long as it lives.
struct StreamGuard(Arc<AtomicUsize>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Host {
    pub fn new(data: PathBuf, exe_dir: Option<PathBuf>, port: u16) -> Self {
        Self {
            data,
            exe_dir,
            port,
            agent: agent(Some(Duration::from_secs(10))),
            stream_agent: agent(None),
            last_activity: Mutex::new(Instant::now()),
            open_streams: Arc::new(AtomicUsize::new(0)),
            quit: AtomicBool::new(false),
            managing: Mutex::new(()),
        }
    }

    /// How long since anyone used the dashboard (zero while a page is open).
    pub fn idle_for(&self) -> Duration {
        if self.open_streams.load(Ordering::Relaxed) > 0 {
            return Duration::ZERO;
        }
        self.last_activity.lock().unwrap().elapsed()
    }

    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    fn origins(&self) -> [String; 2] {
        [format!("http://127.0.0.1:{}", self.port), format!("http://localhost:{}", self.port)]
    }

    /// Requests must be addressed to us by a local name, and changes must come from our page.
    fn allowed(&self, req: &Request) -> Result<(), &'static str> {
        let [a, b] = self.origins();
        let host = req.header("host").unwrap_or_default();
        if !(a.ends_with(&format!("//{host}")) || b.ends_with(&format!("//{host}"))) {
            return Err("this dashboard only answers to 127.0.0.1 or localhost");
        }
        let changes = !matches!(req.method.as_str(), "GET" | "HEAD");
        let origin = req.header("origin");
        if changes && !origin.is_some_and(|o| o == a || o == b) {
            return Err("changes must come from the dashboard page");
        }
        Ok(())
    }

    pub fn handle(&self, req: Request) -> Response {
        *self.last_activity.lock().unwrap() = Instant::now();
        if let Err(why) = self.allowed(&req) {
            return Response::error(403, why);
        }
        let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        match (req.method.as_str(), segments.as_slice()) {
            ("GET", _) if !req.path.starts_with("/api/") => {
                let path = if req.path == "/index.html" { "/" } else { req.path.as_str() };
                match ASSETS.iter().find(|a| a.path == path) {
                    Some(a) => Response::new(200, a.content_type, a.body)
                        .with_header("Cache-Control", "no-cache")
                        .with_header("X-Content-Type-Options", "nosniff")
                        .with_header(
                            "Content-Security-Policy",
                            "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
                        ),
                    None => Response::error(404, "not found"),
                }
            }
            ("GET", ["api", "ping"]) => {
                Response::json(200, &json!({ "app": "photo-stamp-ui", "version": env!("CARGO_PKG_VERSION") }))
            }
            ("POST", ["api", "quit"]) => {
                self.quit.store(true, Ordering::Relaxed);
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    system::log("dashboard stopped");
                    std::process::exit(0);
                });
                Response::ok()
            }
            ("GET", ["api", "services"]) => Response::json(200, &self.services()),
            ("POST", ["api", "start-here"]) => self.start_here(),
            (_, ["api", "s", key, rest @ ..]) => {
                let Some(reg) = Registration::load(&self.data, key) else {
                    return Response::error(404, "no photo-stamp has watched that folder");
                };
                match (req.method.as_str(), rest) {
                    ("POST", ["start"]) => self.start(&reg),
                    ("POST", ["restart"]) => self.restart(&reg),
                    ("GET", ["startup"]) => {
                        Response::json(200, &json!({ "on": system::starts_at_login(&reg.key) }))
                    }
                    ("POST", ["startup"]) => match req.json::<StartupBody>() {
                        Ok(b) => self.set_startup(&reg, b.on),
                        Err(_) => Response::error(400, "expected {\"on\": true|false}"),
                    },
                    _ => self.proxy(&reg, &req, &rest.join("/")),
                }
            }
            (_, ["api", ..]) => Response::error(404, "no such endpoint"),
            _ => Response::error(405, "method not allowed"),
        }
    }

    fn reachable(&self, reg: &Registration) -> bool {
        let Some(port) = reg.port.filter(|_| reg.running) else { return false };
        agent(Some(Duration::from_secs(2)))
            .get(&format!("http://127.0.0.1:{port}/api/settings"))
            .header("Authorization", &format!("Bearer {}", reg.token))
            .call()
            .is_ok_and(|r| r.status() == 200)
    }

    fn here(&self) -> Option<(PathBuf, PathBuf)> {
        let dir = self.exe_dir.clone()?;
        let exe = service_exe(&dir);
        exe.is_file().then_some((dir, exe))
    }

    fn services(&self) -> serde_json::Value {
        let services: Vec<Service> = Registration::all(&self.data)
            .into_iter()
            .map(|r| Service {
                running: self.reachable(&r),
                startup: system::starts_at_login(&r.key),
                can_start: r.exe.as_deref().is_some_and(Path::is_file),
                key: r.key,
                root: r.root,
                outbox: r.outbox,
                version: r.version,
                pid: r.pid,
                started: r.started,
            })
            .collect();
        // photo-stamp beside us, for a folder nothing has watched yet.
        let here = self.here().filter(|(dir, _)| {
            let key = photo_stamp_http::registry::folder_key(dir);
            !services.iter().any(|s| s.key == key)
        });
        json!({
            "services": services,
            "here": here.map(|(dir, _)| dir),
            "version": env!("CARGO_PKG_VERSION"),
        })
    }

    /// Start photo-stamp watching `root`, and wait until it answers.
    fn launch(&self, exe: &Path, root: &Path) -> Response {
        let key = photo_stamp_http::registry::folder_key(root);
        let before = Registration::load(&self.data, &key).map(|r| (r.pid, r.started));
        let args: [&OsStr; 2] = ["--root".as_ref(), root.as_os_str()];
        if let Err(e) = system::spawn_detached(exe, &args) {
            return Response::error(500, &format!("couldn't start {}: {e}", exe.display()));
        }
        system::log(&format!("started {} for {}", exe.display(), root.display()));
        let deadline = Instant::now() + START_WAIT;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(reg) = Registration::load(&self.data, &key)
                && Some((reg.pid, reg.started)) != before
                && self.reachable(&reg)
            {
                return Response::json(200, &json!({ "key": key }));
            }
        }
        Response::error(
            503,
            "photo-stamp was started but hasn't answered yet; see %LOCALAPPDATA%\\photo-stamp\\photo-stamp.log",
        )
    }

    fn start_here(&self) -> Response {
        let _one = self.managing.lock().unwrap();
        match self.here() {
            Some((dir, exe)) => self.launch(&exe, &dir),
            None => Response::error(404, "photo-stamp isn't in the dashboard's folder"),
        }
    }

    fn start(&self, reg: &Registration) -> Response {
        let _one = self.managing.lock().unwrap();
        if self.reachable(reg) {
            return Response::json(200, &json!({ "key": reg.key }));
        }
        let exe = reg.exe.clone().filter(|e| e.is_file()).or_else(|| self.here().map(|(_, exe)| exe));
        match exe {
            Some(exe) => self.launch(&exe, &reg.root),
            None => Response::error(404, "can't find the photo-stamp program for this folder"),
        }
    }

    /// Stop and start again (e.g. after changing the outbox).
    fn restart(&self, reg: &Registration) -> Response {
        {
            let _one = self.managing.lock().unwrap();
            if self.reachable(reg) {
                let _ = self.call(reg, "POST", "stop", b"");
                // It finishes the photo in hand first.
                let deadline = Instant::now() + STOP_WAIT;
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(250));
                    let now = Registration::load(&self.data, &reg.key);
                    if now.is_none_or(|r| !r.running || r.pid != reg.pid) {
                        break;
                    }
                }
            }
        }
        let reg = Registration::load(&self.data, &reg.key).unwrap_or_else(|| reg.clone());
        self.start(&reg)
    }

    fn set_startup(&self, reg: &Registration, on: bool) -> Response {
        let Some(exe) = reg.exe.clone().filter(|e| e.is_file()) else {
            return Response::error(404, "can't find the photo-stamp program for this folder");
        };
        let flag = if on { "--install-startup" } else { "--uninstall-startup" };
        let args: [&OsStr; 3] = [flag.as_ref(), "--root".as_ref(), reg.root.as_os_str()];
        match system::run(&exe, &args) {
            Ok(_) => Response::json(200, &json!({ "on": system::starts_at_login(&reg.key) })),
            Err(e) => Response::error(500, &format!("couldn't change start at login: {e}")),
        }
    }

    fn call(
        &self,
        reg: &Registration,
        method: &str,
        rest: &str,
        body: &[u8],
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let port = reg.port.unwrap_or(0);
        let url = format!("http://127.0.0.1:{port}/api/{rest}");
        let auth = format!("Bearer {}", reg.token);
        match method {
            "POST" => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(body),
            "PUT" => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(body),
            "DELETE" => self.agent.delete(&url).header("Authorization", &auth).call(),
            _ => self.agent.get(&url).header("Authorization", &auth).call(),
        }
    }

    /// Pass a request on to the service's API.
    fn proxy(&self, reg: &Registration, req: &Request, rest: &str) -> Response {
        let not_running = || Response::error(503, "photo-stamp isn't running for this folder");
        let Some(port) = reg.port.filter(|_| reg.running) else { return not_running() };
        let rest = if req.query.is_empty() { rest.to_owned() } else { format!("{rest}?{}", req.query) };

        if rest == "events" && req.method == "GET" {
            let resp = self
                .stream_agent
                .get(&format!("http://127.0.0.1:{port}/api/events"))
                .header("Authorization", &format!("Bearer {}", reg.token))
                .call();
            let Ok(resp) = resp.inspect_err(|e| system::log(&format!("events: {e}"))) else {
                return not_running();
            };
            if resp.status() != 200 {
                return Response::error(502, "photo-stamp refused the event stream");
            }
            self.open_streams.fetch_add(1, Ordering::Relaxed);
            let open = self.open_streams.clone();
            return Response::stream(
                "text/event-stream",
                Box::new(move |out| {
                    let _guard = StreamGuard(open);
                    let mut body = resp.into_body().into_reader();
                    pump(&mut body, out)
                }),
            );
        }

        match self.call(reg, &req.method, &rest, &req.body) {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let cache =
                    resp.headers().get("cache-control").and_then(|v| v.to_str().ok()).map(str::to_owned);
                match resp.body_mut().with_config().limit(32 * 1024 * 1024).read_to_vec() {
                    Ok(body) => {
                        let r = Response::new(status, &content_type, body);
                        match cache {
                            Some(c) => r.with_header("Cache-Control", &c),
                            None => r,
                        }
                    }
                    Err(e) => Response::error(502, &format!("reading photo-stamp's answer: {e}")),
                }
            }
            Err(_) => not_running(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_stamp_http::registry::folder_key;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ps-ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn req(method: &str, path: &str, host: &str, origin: Option<&str>) -> Request {
        let mut headers = vec![("host".to_owned(), host.to_owned())];
        if let Some(o) = origin {
            headers.push(("origin".into(), o.into()));
        }
        Request { method: method.into(), path: path.into(), headers, ..Request::default() }
    }

    /// A stand-in photo-stamp API: checks the token, echoes what it was asked, streams events.
    fn fake_service(data: &Path, root: &str) -> Registration {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            photo_stamp_http::serve(
                listener,
                Arc::new(|req: Request| {
                    if req.header("authorization") != Some("Bearer tok") {
                        return Response::error(401, "token");
                    }
                    if req.path == "/api/events" {
                        return Response::stream(
                            "text/event-stream",
                            Box::new(|out| out.write_all(b"data: {\"type\":\"hello\"}\n\n")),
                        );
                    }
                    Response::json(
                        200,
                        &json!({ "method": req.method, "path": req.path, "query": req.query }),
                    )
                }),
            )
        });
        let reg = Registration {
            key: folder_key(Path::new(root)),
            root: root.into(),
            outbox: Path::new(root).join("outbox"),
            exe: None,
            pid: 42,
            port: Some(port),
            token: "tok".into(),
            version: "0.6.0".into(),
            started: 1,
            running: true,
        };
        reg.save(data).unwrap();
        reg
    }

    fn body_json(r: Response) -> serde_json::Value {
        match r.body {
            photo_stamp_http::Body::Bytes(b) => serde_json::from_slice(&b).unwrap(),
            photo_stamp_http::Body::Stream(_) => panic!("streamed"),
        }
    }

    #[test]
    fn only_local_names_and_our_own_page() {
        let host = Host::new(temp("checks"), None, 8765);
        assert_eq!(host.handle(req("GET", "/api/ping", "127.0.0.1:8765", None)).status, 200);
        assert_eq!(host.handle(req("GET", "/api/ping", "localhost:8765", None)).status, 200);
        assert_eq!(
            host.handle(req("GET", "/api/ping", "evil.example:8765", None)).status,
            403,
            "DNS rebinding"
        );
        assert_eq!(host.handle(req("GET", "/api/ping", "127.0.0.1:9999", None)).status, 403);
        let start = |origin| host.handle(req("POST", "/api/start-here", "127.0.0.1:8765", origin)).status;
        assert_eq!(start(None), 403, "changes need an Origin");
        assert_eq!(start(Some("https://evil.example")), 403, "cross-site");
        assert_eq!(start(Some("http://127.0.0.1:8765")), 404, "allowed, but no photo-stamp here");
    }

    #[test]
    fn serves_the_page() {
        let host = Host::new(temp("page"), None, 8765);
        let r = host.handle(req("GET", "/", "127.0.0.1:8765", None));
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(n, v)| n == "Content-Type" && v.starts_with("text/html")));
        assert_eq!(host.handle(req("GET", "/nope.js", "127.0.0.1:8765", None)).status, 404);
    }

    #[test]
    fn lists_services_and_proxies_with_the_token() {
        let data = temp("proxy");
        let reg = fake_service(&data, "/photos/drop");
        let host = Host::new(data.clone(), None, 8765);

        let list = body_json(host.handle(req("GET", "/api/services", "127.0.0.1:8765", None)));
        assert_eq!(list["services"][0]["root"], "/photos/drop");
        assert_eq!(list["services"][0]["running"], true);

        let mut r = req("GET", &format!("/api/s/{}/results", reg.key), "127.0.0.1:8765", None);
        r.query = "before=10".into();
        let v = body_json(host.handle(r));
        assert_eq!((v["path"].as_str(), v["query"].as_str()), (Some("/api/results"), Some("before=10")));

        let r = req(
            "POST",
            &format!("/api/s/{}/pause", reg.key),
            "127.0.0.1:8765",
            Some("http://127.0.0.1:8765"),
        );
        assert_eq!(body_json(host.handle(r))["method"], "POST");

        assert_eq!(host.handle(req("GET", "/api/s/unknown/status", "127.0.0.1:8765", None)).status, 404);

        // A stopped service is reported as such rather than proxied.
        Registration { running: false, port: None, ..reg.clone() }.save(&data).unwrap();
        assert_eq!(
            host.handle(req("GET", &format!("/api/s/{}/status", reg.key), "127.0.0.1:8765", None)).status,
            503
        );
        let list = body_json(host.handle(req("GET", "/api/services", "127.0.0.1:8765", None)));
        assert_eq!(list["services"][0]["running"], false);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn streams_events_through() {
        let data = temp("events");
        let reg = fake_service(&data, "/photos/drop");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = Arc::new(Host::new(data.clone(), None, port));
        let h = host.clone();
        std::thread::spawn(move || photo_stamp_http::serve(listener, Arc::new(move |r| h.handle(r))));

        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /api/s/{}/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n", reg.key).unwrap();
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        assert!(text.contains("text/event-stream"), "{text}");
        assert!(text.ends_with("data: {\"type\":\"hello\"}\n\n"), "{text}");
        std::fs::remove_dir_all(&data).unwrap();
    }
}
