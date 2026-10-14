//! photo-stamp-server: the place server. Holds the full place data (an Overture Maps mirror plus
//! GeoNames and Wikidata) and names points for the family's photo-stamp clients.

pub mod gazetteer;
pub mod import;
pub mod overture;
pub mod service;
pub mod store;
pub mod updates;
pub mod wkb;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= Level::Info
    }

    fn log(&self, r: &Record) {
        if self.enabled(r.metadata()) {
            let level = match r.level() {
                Level::Error => "error ",
                Level::Warn => "warn  ",
                _ => "",
            };
            eprintln!("{} {level}{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), r.args());
        }
    }

    fn flush(&self) {}
}

/// Log to stderr (the container's log).
pub fn init_logging() {
    static LOGGER: StderrLogger = StderrLogger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(LevelFilter::Info);
}
