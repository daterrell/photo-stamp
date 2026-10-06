//! Watch-folder mode. Under the root:
//!
//! ```text
//! inbox/      drop photos here (subfolders are kept)
//! stamped/    stamped copies appear here
//! originals/  the untouched originals are moved here after stamping
//! skipped/    files that weren't stamped (no metadata, unsupported, already stamped, broken)
//! ```
//!
//! Two mechanisms run side by side: filesystem events (inotify etc.) pick new files up immediately,
//! and polling rescans the inbox every few seconds as a backup for anything the events miss
//! (network shares and Windows drives under WSL don't deliver them; queues can overflow). If
//! events can't be set up at all, polling carries on alone.
//! A file is only processed once its size and mtime have stopped changing, so half-copied
//! files are left alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::WalkDir;

use crate::formats;
use crate::geo::Geocoder;
use crate::render::Renderer;
use crate::stamp::{GeoUnavailable, Prepared, prepare, write_stamped};

/// Place lookups are retried this many times (with growing delays) before settling for coordinates.
const GEO_ATTEMPTS: u32 = 4;
const GEO_RETRY_BASE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Config {
    /// How long a file must stay unchanged before it's processed.
    pub settle: Duration,
    /// How often to poll the inbox, as a backup to filesystem events.
    pub poll_interval: Duration,
    /// Listen for filesystem events (polling always runs too).
    pub events: bool,
    pub quality: u8,
}

pub struct Layout {
    pub inbox: PathBuf,
    pub stamped: PathBuf,
    pub originals: PathBuf,
    pub skipped: PathBuf,
}

impl Layout {
    pub fn new(root: &Path) -> Self {
        Self {
            inbox: root.join("inbox"),
            stamped: root.join("stamped"),
            originals: root.join("originals"),
            skipped: root.join("skipped"),
        }
    }

    fn create(&self) -> anyhow::Result<()> {
        for dir in [&self.inbox, &self.stamped, &self.originals, &self.skipped] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

/// What we last saw of a file in the inbox.
struct Seen {
    len: u64,
    mtime: Option<SystemTime>,
    since: Instant,
}

struct Deferred {
    attempts: u32,
    next_try: Instant,
}

pub struct Inbox<'a> {
    layout: Layout,
    config: Config,
    geo: &'a dyn Geocoder,
    renderer: Renderer,
    seen: HashMap<PathBuf, Seen>,
    deferred: HashMap<PathBuf, Deferred>,
}

impl<'a> Inbox<'a> {
    pub fn new(root: &Path, config: Config, geo: &'a dyn Geocoder) -> anyhow::Result<Self> {
        let layout = Layout::new(root);
        layout.create()?;
        Ok(Self {
            layout,
            config,
            geo,
            renderer: Renderer::new(),
            seen: HashMap::new(),
            deferred: HashMap::new(),
        })
    }

    /// Watch forever.
    pub fn run(self) -> anyhow::Result<()> {
        self.run_until(|| false)
    }

    /// Watch until `stop` returns true (checked after every scan).
    pub fn run_until(mut self, stop: impl Fn() -> bool) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        // Kept alive for the whole loop; dropping it would stop the events.
        let mut _watcher: Option<RecommendedWatcher> = None;
        if self.config.events {
            match start_event_watcher(&self.layout.inbox, tx) {
                Ok(w) => _watcher = Some(w),
                Err(e) => eprintln!("warning: filesystem events unavailable ({e:#}); relying on polling"),
            }
        }
        let mut events = _watcher.is_some();
        println!(
            "watching {} (filesystem events: {}, polling every {:?}, settle {:?})",
            self.layout.inbox.display(),
            if events { "on" } else { "off" },
            self.config.poll_interval,
            self.config.settle,
        );

        loop {
            self.scan_once();
            if stop() {
                return Ok(());
            }
            // Next scan: on an event, when a pending file may have settled, or at the poll interval.
            let wait = if self.seen.is_empty() && self.deferred.is_empty() {
                self.config.poll_interval
            } else {
                self.config.settle.min(self.config.poll_interval).max(Duration::from_millis(250))
            };
            if !events {
                std::thread::sleep(wait);
                continue;
            }
            match rx.recv_timeout(wait) {
                // Error events (e.g. a queue overflow) also just mean "go and look".
                Ok(_) => {
                    // Let a burst of events (a big copy) coalesce before scanning.
                    std::thread::sleep(Duration::from_millis(250));
                    while rx.try_recv().is_ok() {}
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    eprintln!("warning: filesystem events stopped; relying on polling");
                    events = false;
                }
            }
        }
    }

    /// Look through the inbox once and process every file that has settled.
    pub fn scan_once(&mut self) {
        let now = Instant::now();
        let mut present = Vec::new();
        for entry in WalkDir::new(&self.layout.inbox).min_depth(1).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_file() || is_temporary(path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            present.push(path.to_owned());
            let (len, mtime) = (meta.len(), meta.modified().ok());
            let seen = self.seen.entry(path.to_owned()).or_insert(Seen { len, mtime, since: now });
            if seen.len != len || seen.mtime != mtime {
                *seen = Seen { len, mtime, since: now };
            }
        }
        self.seen.retain(|p, _| present.contains(p));
        self.deferred.retain(|p, _| present.contains(p));

        let ready: Vec<PathBuf> = self
            .seen
            .iter()
            .filter(|(p, s)| {
                now.duration_since(s.since) >= self.config.settle
                    && self.deferred.get(*p).is_none_or(|d| now >= d.next_try)
            })
            .map(|(p, _)| p.clone())
            .collect();
        for path in ready {
            self.process(&path);
            if !path.exists() {
                self.seen.remove(&path);
                self.deferred.remove(&path);
                self.remove_empty_parents(&path);
            }
        }
    }

    fn process(&mut self, path: &Path) {
        let rel = path.strip_prefix(&self.layout.inbox).unwrap_or(path).to_owned();
        let attempts = self.deferred.get(path).map_or(0, |d| d.attempts);
        let strict_geo = attempts + 1 < GEO_ATTEMPTS;

        let result = (|| -> anyhow::Result<String> {
            match prepare(path, self.geo, strict_geo)? {
                Prepared::Skip(why) => {
                    let to = move_unique(path, &self.layout.skipped.join(&rel))?;
                    Ok(format!("skipped  {}: {why} → {}", rel.display(), to.display()))
                }
                Prepared::Stamp(job) => {
                    let out_rel = rel.with_file_name(formats::output_name(path, job.kind));
                    let dest = unique_path(&self.layout.stamped.join(&out_rel));
                    if let Some(dir) = dest.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    write_stamped(path, &job, &mut self.renderer, &dest, self.config.quality)?;
                    move_unique(path, &self.layout.originals.join(&rel))?;
                    Ok(format!("stamped  {}: {} → {}", rel.display(), job.lines.join(" | "), dest.display()))
                }
            }
        })();

        match result {
            Ok(msg) => println!("{msg}"),
            Err(e) if e.downcast_ref::<GeoUnavailable>().is_some() => {
                let delay = GEO_RETRY_BASE * 2u32.pow(attempts);
                println!("waiting  {}: {e:#}; retrying in {delay:?}", rel.display());
                self.deferred.insert(
                    path.to_owned(),
                    Deferred { attempts: attempts + 1, next_try: Instant::now() + delay },
                );
            }
            Err(e) => {
                eprintln!("error    {}: {e:#}", rel.display());
                match move_unique(path, &self.layout.skipped.join(&rel)) {
                    Ok(to) => eprintln!("         moved to {}", to.display()),
                    Err(e2) => eprintln!("         and couldn't move it out of the inbox: {e2:#}"),
                }
            }
        }
    }

    fn remove_empty_parents(&self, path: &Path) {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == self.layout.inbox || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}

fn start_event_watcher(
    dir: &Path,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
) -> notify::Result<RecommendedWatcher> {
    let mut w = notify::recommended_watcher(tx)?;
    w.watch(dir, RecursiveMode::Recursive)?;
    Ok(w)
}

/// Partial downloads, editor temp files, our own temp files, dotfiles.
fn is_temporary(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.')
        || name.starts_with('~')
        || [".part", ".tmp", ".crdownload", ".download", ".partial"].iter().any(|s| name.ends_with(s))
}

/// `path`, or "name (2).ext", "name (3).ext"… if it's taken.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

/// Move `from` to `to` (or a free variant of it), creating folders as needed.
fn move_unique(from: &Path, to: &Path) -> anyhow::Result<PathBuf> {
    let to = unique_path(to);
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, &to).is_err() {
        // Different filesystems: copy then delete.
        std::fs::copy(from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        std::fs::remove_file(from)?;
    }
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::tests::{EIFFEL_TAGS, FailingGeocoder, FixedPlace, make_photo, temp_dir};

    fn config() -> Config {
        Config { settle: Duration::ZERO, poll_interval: Duration::from_secs(1), events: true, quality: 90 }
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().strip_prefix(dir).unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn sorts_inbox_into_folders() {
        let root = temp_dir("watch");
        let mut inbox = Inbox::new(&root, config(), &FixedPlace).unwrap();
        let trip = root.join("inbox/trip");
        std::fs::create_dir_all(&trip).unwrap();
        make_photo(&trip.join("a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("inbox/b.png"), EIFFEL_TAGS);
        make_photo(&root.join("inbox/bare.jpg"), &[]);
        std::fs::write(root.join("inbox/readme.txt"), "x").unwrap();
        std::fs::write(root.join("inbox/.hidden.jpg.part"), "x").unwrap();
        // Same name already in stamped/: the new one must not overwrite it.
        std::fs::write(root.join("stamped/b.png"), "older").unwrap();

        inbox.scan_once();

        assert_eq!(files_under(&root.join("inbox")), [".hidden.jpg.part"]);
        assert_eq!(files_under(&root.join("stamped")), ["b (2).png", "b.png", "trip/a.jpg"]);
        assert_eq!(files_under(&root.join("originals")), ["b.png", "trip/a.jpg"]);
        assert_eq!(files_under(&root.join("skipped")), ["bare.jpg", "readme.txt"]);
        assert!(!trip.exists(), "emptied inbox subfolder should be removed");
        assert!(crate::metadata::read(&root.join("stamped/trip/a.jpg")).unwrap().already_stamped);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn waits_for_files_to_settle() {
        let root = temp_dir("settle");
        let cfg = Config { settle: Duration::from_secs(3600), ..config() };
        let mut inbox = Inbox::new(&root, cfg, &FixedPlace).unwrap();
        make_photo(&root.join("inbox/a.jpg"), EIFFEL_TAGS);
        inbox.scan_once();
        assert_eq!(files_under(&root.join("inbox")), ["a.jpg"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn geo_outage_defers_instead_of_using_coordinates() {
        let root = temp_dir("defer");
        let mut inbox = Inbox::new(&root, config(), &FailingGeocoder).unwrap();
        make_photo(&root.join("inbox/a.jpg"), EIFFEL_TAGS);
        inbox.scan_once();
        assert_eq!(files_under(&root.join("inbox")), ["a.jpg"], "should wait for the network");
        assert_eq!(inbox.deferred[&root.join("inbox/a.jpg")].attempts, 1);

        // Out of retries: stamp with coordinates rather than wait forever.
        inbox.deferred.get_mut(&root.join("inbox/a.jpg")).unwrap().attempts = GEO_ATTEMPTS;
        inbox.deferred.get_mut(&root.join("inbox/a.jpg")).unwrap().next_try = Instant::now();
        inbox.scan_once();
        assert_eq!(files_under(&root.join("stamped")), ["a.jpg"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Run the watcher, drop a photo into the inbox once it's running, and return how long
    /// it took to be stamped (the watcher stops as soon as it is, or after 20 s).
    fn time_to_stamp(config: Config) -> Duration {
        let root = temp_dir(&format!("live-{}-{}", config.events, config.poll_interval.as_millis()));
        let staged = root.join("staged.jpg");
        make_photo(&staged, EIFFEL_TAGS);
        let inbox = Inbox::new(&root, config, &FixedPlace).unwrap();
        let dropped = root.join("inbox/a.jpg");
        let stamped = root.join("stamped/a.jpg");
        let start = Instant::now();
        let dropper = std::thread::spawn({
            let (staged, dropped) = (staged.clone(), dropped.clone());
            // Rename in, so the watcher never sees a half-written file.
            move || {
                std::thread::sleep(Duration::from_millis(500));
                std::fs::rename(staged, dropped).unwrap();
            }
        });
        inbox.run_until(|| stamped.exists() || start.elapsed() > Duration::from_secs(20)).unwrap();
        dropper.join().unwrap();
        assert!(stamped.exists(), "never stamped");
        std::fs::remove_dir_all(&root).unwrap();
        start.elapsed()
    }

    #[test]
    fn events_pick_files_up_without_waiting_for_a_poll() {
        let took = time_to_stamp(Config { poll_interval: Duration::from_secs(60), ..config() });
        assert!(took < Duration::from_secs(10), "took {took:?}: events not working");
    }

    #[test]
    fn polling_alone_still_picks_files_up() {
        let took =
            time_to_stamp(Config { events: false, poll_interval: Duration::from_millis(200), ..config() });
        assert!(took < Duration::from_secs(10), "took {took:?}");
    }

    #[test]
    fn unique_names() {
        let dir = temp_dir("unique");
        let p = dir.join("x.jpg");
        assert_eq!(unique_path(&p), p);
        std::fs::write(&p, "").unwrap();
        assert_eq!(unique_path(&p), dir.join("x (2).jpg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
