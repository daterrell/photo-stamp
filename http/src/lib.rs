//! A minimal HTTP/1.1 server for photo-stamp's local API and dashboard.
//!
//! Each connection carries one request and is closed after the response (`Connection: close`),
//! which keeps this small and makes streamed responses (server-sent events) simple: the body is
//! written straight to the socket, flushed as it goes, and ends when the connection closes.
//! Only meant for localhost: there's no TLS, keep-alive or chunked request bodies.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub mod registry;

/// The version of photo-stamp: from the git tag (`v0.7.0` → `0.7.0`), or between releases
/// something like `0.6.1-dev.7+g1b37dbe` (see `scripts/version.sh`).
pub const VERSION: &str = env!("PHOTO_STAMP_VERSION");

/// Largest request body accepted.
const MAX_BODY: usize = 1024 * 1024;
const MAX_HEADER_LINES: usize = 100;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    /// Path without the query string, e.g. `/api/status`.
    pub path: String,
    /// The query string without `?`.
    pub query: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// A query parameter (percent-decoded).
    pub fn param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| percent_decode(v))
        })
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

/// Writes a streamed body; return when done (or when writing fails, e.g. the client left).
pub type Stream = Box<dyn FnOnce(&mut dyn Write) -> io::Result<()> + Send>;

pub enum Body {
    Bytes(Vec<u8>),
    Stream(Stream),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: Body::Bytes(body.into()),
        }
    }

    pub fn json(status: u16, value: &impl serde::Serialize) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self::new(status, "application/json", body),
            Err(e) => Self::error(500, &e.to_string()),
        }
    }

    /// `{"error": message}`.
    pub fn error(status: u16, message: &str) -> Self {
        Self::new(status, "application/json", serde_json::json!({ "error": message }).to_string())
    }

    pub fn ok() -> Self {
        Self::new(200, "application/json", "{}")
    }

    /// A body written as it's produced, e.g. server-sent events.
    pub fn stream(content_type: &str, write: Stream) -> Self {
        Self {
            status: 200,
            headers: vec![
                ("Content-Type".into(), content_type.into()),
                ("Cache-Control".into(), "no-cache".into()),
            ],
            body: Body::Stream(write),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Serve `listener` forever, a thread per connection.
pub fn serve(listener: TcpListener, handler: Handler) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let handler = handler.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &handler);
        });
    }
}

/// A server running on its own thread, which can be stopped (freeing its port).
pub struct Server {
    addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Serve `listener` on a background thread until [`Server::stop`].
    pub fn spawn(listener: TcpListener, handler: Handler) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = std::thread::Builder::new().name("http".into()).spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &handler);
                });
            }
        })?;
        Ok(Self { addr, stopping, thread: Some(thread) })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Stop accepting connections and close the listening socket. Requests in progress finish on
    /// their own (long-lived streams should watch for shutdown themselves).
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        // Wake the accept() so the loop sees the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn handle(stream: TcpStream, handler: &Handler) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    let mut out = stream.try_clone()?;
    let response = match read_request(&mut BufReader::new(stream)) {
        Ok(req) => handler(req),
        Err(e) => Response::error(400, &e.to_string()),
    };
    write_response(&mut out, response)
}

pub fn read_request(r: &mut impl BufRead) -> io::Result<Request> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(bad("bad request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request {
        method: method.to_owned(),
        path: percent_decode(path),
        query: query.to_owned(),
        ..Request::default()
    };
    for _ in 0..MAX_HEADER_LINES {
        line.clear();
        r.read_line(&mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((name, value)) = l.split_once(':') {
            req.headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(bad("request body too large"));
    }
    req.body = vec![0; len];
    r.read_exact(&mut req.body)?;
    Ok(req)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

pub fn write_response(out: &mut impl Write, response: Response) -> io::Result<()> {
    let mut head =
        format!("HTTP/1.1 {} {}\r\nConnection: close\r\n", response.status, reason(response.status));
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match response.body {
        Body::Bytes(body) => {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            out.write_all(head.as_bytes())?;
            out.write_all(&body)?;
            out.flush()
        }
        Body::Stream(write) => {
            head.push_str("\r\n");
            out.write_all(head.as_bytes())?;
            out.flush()?;
            write(out)
        }
    }
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One server-sent event (`data: …` plus a blank line).
pub fn sse_event(data: &str) -> String {
    let mut s = String::new();
    for line in data.lines() {
        s.push_str("data: ");
        s.push_str(line);
        s.push('\n');
    }
    s.push('\n');
    s
}

/// Connect the body of `from` (e.g. an upstream response) to `to`, flushing as data arrives.
pub fn pump(from: &mut dyn Read, to: &mut dyn Write) -> io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_a_request() {
        let raw = "POST /api/s/abc/retry?x=1&name=a%20b+c HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\
                   Content-Length: 4\r\nOrigin: http://127.0.0.1:8765\r\n\r\n{\"a\"";
        let req = read_request(&mut Cursor::new(raw)).unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/api/s/abc/retry"));
        assert_eq!(req.header("HOST"), Some("127.0.0.1:8765"));
        assert_eq!(req.param("name").as_deref(), Some("a b c"));
        assert_eq!(req.param("missing"), None);
        assert_eq!(req.body, b"{\"a\"");
    }

    #[test]
    fn writes_fixed_and_streamed_responses() {
        let mut out = Vec::new();
        write_response(&mut out, Response::json(200, &serde_json::json!({"ok": true}))).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"ok\":true}"));

        let mut out = Vec::new();
        let resp = Response::stream(
            "text/event-stream",
            Box::new(|w| {
                w.write_all(sse_event("{\"n\":1}").as_bytes())?;
                w.write_all(sse_event("two\nlines").as_bytes())
            }),
        );
        write_response(&mut out, resp).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Content-Length"));
        assert!(text.ends_with("\r\n\r\ndata: {\"n\":1}\n\ndata: two\ndata: lines\n\n"));
    }

    #[test]
    fn a_stopped_server_frees_its_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = Server::spawn(listener, Arc::new(|_| Response::ok())).unwrap();
        let port = server.port();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("HTTP/1.1 200"));
        server.stop();
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok(), "port {port} is free again");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("trip%2Fday%201%2F%E6%9D%B1.jpg"), "trip/day 1/東.jpg");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn serves_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            serve(listener, Arc::new(|req: Request| Response::new(200, "text/plain", req.path)))
        });
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        assert!(text.ends_with("/hello"), "{text}");
    }
}
