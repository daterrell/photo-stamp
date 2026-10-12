//! The service's local API, for the dashboard (`photo-stamp-ui`). It listens on 127.0.0.1 on a
//! random port and requires the token from this service's registration file
//! (`services/<folder>.json` in the data folder), so only this user's programs can use it.
//!
//! - `GET  /api/status`: everything at once ([`crate::status::Snapshot`])
//! - `GET  /api/events`: server-sent events as things happen
//! - `GET  /api/results?before=<id>&limit=<n>`: older results, newest first
//! - `GET  /api/thumb/<id>/<before|after>`: small previews of stamped photos
//! - `POST /api/pause`, `/api/resume`, `/api/stop`
//! - `POST /api/retry` `{"path": "<path within the watched folder>"}`
//! - `GET|PUT /api/settings`: PUT replies `{"settings": …, "restart": bool}`; outbox and offline
//!   changes only apply after a restart.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use photo_stamp_http::registry::same_secret;
use photo_stamp_http::{Request, Response, serve, sse_event};
use serde::Deserialize;
use serde_json::json;

use crate::outbox::Wake;
use crate::settings::Settings;
use crate::status::{Status, Which};

/// How often an idle event stream sends a comment, so dead connections are noticed.
const KEEPALIVE: Duration = Duration::from_secs(15);

pub struct Api {
    pub status: Arc<Status>,
    pub waker: Mutex<Sender<Wake>>,
    pub token: String,
    /// Where settings changes are saved (`None`: not saved).
    pub settings_path: Option<PathBuf>,
}

/// A random 128-bit token, as hex.
pub fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for the API token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Listen on 127.0.0.1 (a free port) in the background; returns the port.
pub fn start(api: Arc<Api>) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new()
        .name("api".into())
        .spawn(move || serve(listener, Arc::new(move |req| handle(&api, req))))?;
    Ok(port)
}

#[derive(Deserialize)]
struct RetryBody {
    path: String,
}

/// A path within the watched folder, without any way out of it.
fn safe_relative(p: &str) -> Option<PathBuf> {
    let path = Path::new(p);
    path.components().all(|c| matches!(c, Component::Normal(_))).then(|| path.to_owned())
}

pub fn handle(api: &Api, req: Request) -> Response {
    let authorized = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|t| same_secret(t.trim(), &api.token));
    if !authorized {
        return Response::error(401, "missing or wrong token");
    }
    let wake = |w: Wake| {
        let _ = api.waker.lock().unwrap().send(w);
    };
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["api", "status"]) => api.status.snapshot(|s| Response::json(200, s)),
        ("GET", ["api", "events"]) => events(api.status.clone()),
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
        ("POST", ["api", "pause"]) => {
            update_settings(api, |s| s.paused = true);
            Response::ok()
        }
        ("POST", ["api", "resume"]) => {
            update_settings(api, |s| s.paused = false);
            wake(Wake::Fs);
            Response::ok()
        }
        ("POST", ["api", "stop"]) => {
            log::info!("stop requested from the dashboard");
            wake(Wake::Stop);
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
        ("GET", ["api", "settings"]) => Response::json(200, &api.status.settings()),
        ("PUT", ["api", "settings"]) => match req.json::<Settings>() {
            Ok(new) => {
                let new = new.sanitized();
                let restart = api.status.settings().needs_restart(&new);
                update_settings(api, |s| *s = new.clone());
                wake(Wake::Fs);
                Response::json(200, &json!({ "settings": new, "restart": restart }))
            }
            Err(e) => Response::error(400, &format!("bad settings: {e}")),
        },
        (_, ["api", ..]) => Response::error(404, "no such endpoint"),
        _ => Response::error(404, "not found"),
    }
}

/// Change the settings, apply them and save them.
fn update_settings(api: &Api, change: impl FnOnce(&mut Settings)) {
    let mut s = api.status.settings();
    let was_paused = s.paused;
    change(&mut s);
    if s.paused != was_paused {
        log::info!("{} from the dashboard", if s.paused { "paused" } else { "resumed" });
    }
    api.status.set_settings(s.clone());
    if s.paused != was_paused {
        api.status.set_paused(s.paused);
    }
    if let Some(path) = &api.settings_path
        && let Err(e) = s.save(path)
    {
        log::warn!("couldn't save settings to {}: {e}", path.display());
    }
}

fn events(status: Arc<Status>) -> Response {
    let rx = status.subscribe();
    Response::stream(
        "text/event-stream",
        Box::new(move |out: &mut dyn Write| {
            // Tell the client which event the stream starts after, so it can line up a snapshot.
            let seq = status.snapshot(|s| s.seq);
            out.write_all(sse_event(&json!({ "type": "hello", "seq": seq }).to_string()).as_bytes())?;
            out.flush()?;
            loop {
                match rx.recv_timeout(KEEPALIVE) {
                    Ok(msg) => out.write_all(sse_event(&msg).as_bytes())?,
                    Err(RecvTimeoutError::Timeout) => out.write_all(b": keepalive\n\n")?,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                out.flush()?;
            }
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
        (Api { status, waker: Mutex::new(tx), token: "secret".into(), settings_path: None }, rx)
    }

    fn req(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: vec![("authorization".into(), "Bearer secret".into())],
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
    fn requires_the_token() {
        let (api, _rx) = api();
        let mut r = req("GET", "/api/status", "");
        r.headers.clear();
        assert_eq!(handle(&api, r).status, 401);
        let mut r = req("GET", "/api/status", "");
        r.headers = vec![("authorization".into(), "Bearer wrong!".into())];
        assert_eq!(handle(&api, r).status, 401);
        assert_eq!(handle(&api, req("GET", "/api/status", "")).status, 200);
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
        let port = start(api.clone()).unwrap();
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET /api/events HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n").unwrap();
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
    }
}
