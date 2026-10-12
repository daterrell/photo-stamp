//! A small logger: timestamped lines to a file (rotated at 5 MB) and/or stderr.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use log::{Level, LevelFilter, Log, Metadata, Record};

const ROTATE_AT: u64 = 5 * 1024 * 1024;

/// Also receives every line (level, text), e.g. for the dashboard's log view.
pub type Sink = Box<dyn Fn(&'static str, &str) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

/// Send log lines to `sink` too (once; later calls are ignored).
pub fn set_sink(sink: Sink) {
    let _ = SINK.set(sink);
}

struct Logger {
    file: Option<Mutex<File>>,
    stderr: bool,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= Level::Info
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let level = match r.level() {
            Level::Error => "error",
            Level::Warn => "warn ",
            _ => "     ",
        };
        let text = r.args().to_string();
        let line = format!("{} {level} {text}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
        if let Some(sink) = SINK.get() {
            sink(
                match r.level() {
                    Level::Error => "error",
                    Level::Warn => "warn",
                    _ => "info",
                },
                &text,
            );
        }
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap().write_all(line.as_bytes());
        }
        if self.stderr {
            eprint!("{line}");
        }
    }

    fn flush(&self) {
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap().flush();
        }
    }
}

/// Install the logger. With `file`, the previous log is moved to `<name>.1` once it passes 5 MB.
pub fn init(file: Option<&Path>, stderr: bool) -> anyhow::Result<()> {
    let file = match file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if std::fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_AT) {
                let _ = std::fs::rename(path, path.with_extension("log.1"));
            }
            Some(Mutex::new(OpenOptions::new().create(true).append(true).open(path)?))
        }
        None => None,
    };
    // Lives for the rest of the program.
    let logger: &'static Logger = Box::leak(Box::new(Logger { file, stderr }));
    log::set_logger(logger).map_err(|e| anyhow::anyhow!("{e}"))?;
    log::set_max_level(LevelFilter::Info);
    Ok(())
}
