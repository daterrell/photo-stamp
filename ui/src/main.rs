// A background app on Windows: no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! photo-stamp-ui: the dashboard. Serves a page on http://127.0.0.1:8765 that shows and controls
//! the photo-stamp services on this PC. It's independent of them: it finds each one through its
//! registration file (see `photo_stamp_http::registry`), talks to its local API with the token
//! from that file, and can start a service that isn't running.
//!
//! Double-clicking it opens the dashboard in the browser (starting the server if it isn't already
//! running). It exits by itself after half an hour with no dashboard open.

mod host;
mod system;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_PORT: u16 = 8765;
/// Exit after this long with no dashboard open and no requests.
const IDLE_EXIT: Duration = Duration::from_secs(30 * 60);

struct Args {
    port: u16,
    open: bool,
    stop: bool,
}

const USAGE: &str = "\
photo-stamp-ui: the photo-stamp dashboard, in your browser.

Usage: photo-stamp-ui [--port <port>] [--no-open] [--stop]

  --port <port>  port on 127.0.0.1 to serve the dashboard on (default 8765)
  --no-open      don't open the browser
  --stop         stop the dashboard server (photo-stamp itself keeps running)
";

fn parse_args() -> Result<Args, String> {
    let mut args = Args { port: DEFAULT_PORT, open: true, stop: false };
    if let Some(p) = std::env::var("PHOTO_STAMP_UI_PORT").ok().and_then(|p| p.parse().ok()) {
        args.port = p;
    }
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => args.port = it.next().and_then(|p| p.parse().ok()).ok_or("--port needs a number")?,
            "--no-open" => args.open = false,
            "--stop" => args.stop = true,
            "-h" | "--help" => return Err(USAGE.to_owned()),
            "-V" | "--version" => return Err(format!("photo-stamp-ui {}", photo_stamp_http::VERSION)),
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            system::tell(&msg);
            return;
        }
    };
    let url = format!("http://127.0.0.1:{}/", args.port);
    let data = photo_stamp_http::registry::data_dir();
    system::init_log(data.as_deref());

    if args.stop {
        let said = match host::ask_to_quit(args.port) {
            true => "Stopped the photo-stamp dashboard.",
            false => "The photo-stamp dashboard isn't running.",
        };
        system::tell(said);
        return;
    }

    let listener = match TcpListener::bind(("127.0.0.1", args.port)) {
        Ok(l) => l,
        Err(e) => {
            // Already running (another copy of us): just show it.
            if host::is_dashboard(args.port) {
                if args.open {
                    system::open_browser(&url);
                }
                return;
            }
            let msg = format!("Can't start the photo-stamp dashboard on port {}: {e}", args.port);
            system::log(&msg);
            system::tell(&msg);
            return;
        }
    };
    let Some(data) = data else {
        system::tell("Can't find photo-stamp's data folder (%LOCALAPPDATA% isn't set).");
        return;
    };
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
    let host = Arc::new(host::Host::new(data, exe_dir, args.port));
    system::log(&format!("dashboard at {url}"));
    if args.open {
        system::open_browser(&url);
    }
    {
        let host = host.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                if host.idle_for() >= IDLE_EXIT || host.quit_requested() {
                    system::log("dashboard closed");
                    std::process::exit(0);
                }
            }
        });
    }
    photo_stamp_http::serve(listener, Arc::new(move |req| host.handle(req)));
}
