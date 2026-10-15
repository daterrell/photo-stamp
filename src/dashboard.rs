//! The dashboard page, served by photo-stamp itself on http://127.0.0.1:8765 (see [`crate::api`]).
//!
//! Only this PC's browser can use it: requests must be addressed to 127.0.0.1/localhost (which
//! stops other web sites reaching it through DNS tricks), and anything that changes something
//! must come from the dashboard page itself (its `Origin`).

use photo_stamp_http::{Request, Response};

/// Where the dashboard is served unless `PHOTO_STAMP_UI_PORT` or `--port` says otherwise.
pub const DEFAULT_PORT: u16 = 8765;

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

/// The address of the dashboard on `port`.
pub fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

/// Requests must be addressed to us by a local name, and changes must come from our page.
pub fn allowed(port: u16, req: &Request) -> Result<(), &'static str> {
    let origins = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")];
    let host = req.header("host").unwrap_or_default();
    if !origins.iter().any(|o| o.ends_with(&format!("//{host}"))) {
        return Err("the dashboard only answers to 127.0.0.1 or localhost");
    }
    let changes = !matches!(req.method.as_str(), "GET" | "HEAD");
    if changes && !req.header("origin").is_some_and(|o| origins.iter().any(|a| a == o)) {
        return Err("changes must come from the dashboard page");
    }
    Ok(())
}

/// The page and its files.
pub fn asset(path: &str) -> Response {
    let path = if path == "/index.html" { "/" } else { path };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, host: &str, origin: Option<&str>) -> Request {
        let mut headers = vec![("host".to_owned(), host.to_owned())];
        if let Some(o) = origin {
            headers.push(("origin".into(), o.into()));
        }
        Request { method: method.into(), path: "/api/x".into(), headers, ..Request::default() }
    }

    #[test]
    fn only_local_names_and_our_own_page() {
        assert!(allowed(8765, &req("GET", "127.0.0.1:8765", None)).is_ok());
        assert!(allowed(8765, &req("GET", "localhost:8765", None)).is_ok());
        assert!(allowed(8765, &req("GET", "evil.example:8765", None)).is_err(), "DNS rebinding");
        assert!(allowed(8765, &req("GET", "127.0.0.1:9999", None)).is_err());
        assert!(allowed(8765, &req("POST", "127.0.0.1:8765", None)).is_err(), "changes need an Origin");
        assert!(allowed(8765, &req("POST", "127.0.0.1:8765", Some("https://evil.example"))).is_err());
        assert!(allowed(8765, &req("POST", "127.0.0.1:8765", Some("http://127.0.0.1:8765"))).is_ok());
    }

    #[test]
    fn serves_the_page() {
        let r = asset("/");
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(n, v)| n == "Content-Type" && v.starts_with("text/html")));
        assert_eq!(asset("/index.html").status, 200);
        assert_eq!(asset("/nope.js").status, 404);
    }
}
