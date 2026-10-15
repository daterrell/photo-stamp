//! Showing things to the person: the dashboard in their browser, a folder in Explorer.
//!
//! `PHOTO_STAMP_OPEN_LOG=<file>` (for tests) appends what would be opened to that file instead,
//! so test runs don't open windows on the desktop.

use std::io::Write;

/// Open `target` (a web address or a folder) the way double-clicking it would.
pub fn open(target: &str) {
    if let Some(log) = std::env::var_os("PHOTO_STAMP_OPEN_LOG") {
        let line = format!("{target}\n");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = f.write_all(line.as_bytes());
        }
        return;
    }
    #[cfg(windows)]
    crate::winsys::shell_open(target);
    #[cfg(not(windows))]
    {
        use std::process::{Command, Stdio};
        if let Err(e) =
            Command::new("xdg-open").arg(target).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        {
            log::info!("open {target} ({e})");
        }
    }
}
