//! In-place mode: watch a folder tree and replace each *new* photo with the same photo plus the
//! stamp, in the same place under the same name. Camera RAW files are never modified; a stamped
//! `<name>.jpg` is written beside them instead.
//!
//! "New" means created after the watcher last ran (a heartbeat is kept on disk), so photos already
//! in the folder are left alone, while photos that arrived while the PC was off are still caught.
//! Creation time is used rather than modification time because copying in Explorer keeps a photo's
//! original modified date, and an old photo edited later must not suddenly be stamped.
//!
//! As in the inbox watcher, filesystem events give instant pickup and polling is the backup; a
//! file is only processed once it has stopped changing.

use std::collections::HashMap;
use std::fs::{FileTimes, Metadata};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use log::{info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

use crate::formats::{self, Kind};
use crate::geo::Geocoder;
use crate::render::Renderer;
use crate::stamp::{GeoUnavailable, Prepared, prepare, write_stamped};
use crate::watch::is_temporary;

/// Folder beside the exe holding bundled helpers (exiftool); never scanned.
pub const SUPPORT_DIR: &str = "photo-stamp-files";
const SKIP_DIRS: &[&str] = &[SUPPORT_DIR, "$RECYCLE.BIN", "System Volume Information"];

/// Place lookups are retried this many times (with growing delays) before settling for less.
const GEO_ATTEMPTS: u32 = 4;
const GEO_RETRY_BASE: Duration = Duration::from_secs(60);
/// A file another program has open is retried this often, this many times.
const LOCKED_RETRY: Duration = Duration::from_secs(30);
const LOCKED_ATTEMPTS: u32 = 20;
const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Config {
    /// How long a file must stay unchanged before it's processed.
    pub settle: Duration,
    /// How often to poll the tree, as a backup to filesystem events.
    pub poll_interval: Duration,
    /// Listen for filesystem events (polling always runs too).
    pub events: bool,
    pub quality: u8,
    /// Where to keep the heartbeat; `None` means "everything created from now on is new".
    pub state_file: Option<PathBuf>,
}

/// Why the watcher woke up.
pub enum Wake {
    Fs,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Fingerprint {
    len: u64,
    mtime: Option<SystemTime>,
}

impl Fingerprint {
    fn of(meta: &Metadata) -> Self {
        Self { len: meta.len(), mtime: meta.modified().ok() }
    }
}

struct Pending {
    fp: Fingerprint,
    since: Instant,
}

struct Deferred {
    geo_attempts: u32,
    lock_attempts: u32,
    next_try: Instant,
}

#[derive(Serialize, Deserialize)]
struct State {
    /// Unix seconds when the watcher was last known to be running.
    heartbeat: u64,
}

pub struct InPlace<'a> {
    root: PathBuf,
    config: Config,
    geo: &'a dyn Geocoder,
    renderer: Renderer,
    /// Files created before this are "old" and left alone.
    since: SystemTime,
    last_heartbeat: Instant,
    pending: HashMap<PathBuf, Pending>,
    /// Files we've finished with (stamped, or left alone), as we last saw them.
    settled: HashMap<PathBuf, Fingerprint>,
    deferred: HashMap<PathBuf, Deferred>,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl<'a> InPlace<'a> {
    pub fn new(root: &Path, config: Config, geo: &'a dyn Geocoder) -> anyhow::Result<Self> {
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        let now = SystemTime::now();
        let since = config
            .state_file
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<State>(&s).ok())
            .map(|s| UNIX_EPOCH + Duration::from_secs(s.heartbeat))
            .filter(|t| *t < now)
            .unwrap_or(now);
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut me = Self {
            root: root.to_owned(),
            config,
            geo,
            renderer: Renderer::new(),
            since,
            last_heartbeat: Instant::now(),
            pending: HashMap::new(),
            settled: HashMap::new(),
            deferred: HashMap::new(),
            wake_tx,
            wake_rx,
        };
        me.heartbeat();
        Ok(me)
    }

    /// Something that can wake the watcher, e.g. to stop it from another thread.
    pub fn waker(&self) -> mpsc::Sender<Wake> {
        self.wake_tx.clone()
    }

    /// Photos created before this are left alone.
    pub fn since(&self) -> SystemTime {
        self.since
    }

    fn heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
        let Some(path) = &self.config.state_file else { return };
        let state = State { heartbeat: unix_secs(SystemTime::now()) };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&state)?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            warn!("couldn't save state {}: {e}", path.display());
        }
    }

    /// Watch until told to stop (a [`Wake::Stop`] from [`Self::waker`]).
    pub fn run(mut self) -> anyhow::Result<()> {
        let _watcher: Option<RecommendedWatcher> = if self.config.events {
            let tx = self.wake_tx.clone();
            let started = notify::recommended_watcher(move |_| {
                let _ = tx.send(Wake::Fs);
            })
            .and_then(|mut w| w.watch(&self.root, RecursiveMode::Recursive).map(|()| w));
            match started {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!("filesystem events unavailable ({e}); relying on polling");
                    None
                }
            }
        } else {
            None
        };
        info!(
            "watching {} and its subfolders (filesystem events: {}, polling every {:?}); \
             photos created since {} will be stamped in place",
            self.root.display(),
            if _watcher.is_some() { "on" } else { "off" },
            self.config.poll_interval,
            chrono::DateTime::<chrono::Local>::from(self.since).format("%Y-%m-%d %H:%M:%S"),
        );

        loop {
            self.scan_once();
            if self.last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
                self.heartbeat();
            }
            let wait = if self.pending.is_empty() && self.deferred.is_empty() {
                self.config.poll_interval
            } else {
                self.config.settle.min(self.config.poll_interval).max(Duration::from_millis(250))
            };
            match self.wake_rx.recv_timeout(wait.min(HEARTBEAT_EVERY)) {
                Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.heartbeat();
                    info!("stopped");
                    return Ok(());
                }
                Ok(Wake::Fs) => {
                    // Let a burst of events (a big copy) coalesce before scanning.
                    std::thread::sleep(Duration::from_millis(250));
                    while let Ok(w) = self.wake_rx.try_recv() {
                        if matches!(w, Wake::Stop) {
                            let _ = self.wake_tx.send(Wake::Stop);
                            break;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn is_new(&self, meta: &Metadata) -> bool {
        meta.created().or_else(|_| meta.modified()).is_ok_and(|t| t >= self.since)
    }

    /// Look through the tree once and process every new photo that has settled.
    pub fn scan_once(&mut self) {
        let now = Instant::now();
        let mut present: Vec<PathBuf> = Vec::new();
        let walker = WalkDir::new(&self.root).min_depth(1).into_iter().filter_entry(|e| !skipped_dir(e));
        for entry in walker.filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_file() || is_temporary(path) || formats::classify(path).is_none() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let fp = Fingerprint::of(&meta);
            if self.settled.get(path) == Some(&fp) || !self.is_new(&meta) {
                continue;
            }
            present.push(path.to_owned());
            let p = self.pending.entry(path.to_owned()).or_insert(Pending { fp, since: now });
            if p.fp != fp {
                *p = Pending { fp, since: now };
            }
        }
        self.pending.retain(|p, _| present.contains(p));
        self.deferred.retain(|p, _| present.contains(p));

        let mut ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(p, s)| {
                now.duration_since(s.since) >= self.config.settle
                    && self.deferred.get(*p).is_none_or(|d| now >= d.next_try)
            })
            .map(|(p, _)| p.clone())
            .collect();
        ready.sort();
        for path in ready {
            self.process(&path);
        }
    }

    fn rel<'p>(&self, path: &'p Path) -> std::borrow::Cow<'p, str> {
        path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy()
    }

    /// Done with `path` (and anything we wrote) until it changes again.
    fn settle(&mut self, path: &Path) {
        self.pending.remove(path);
        self.deferred.remove(path);
        if let Ok(meta) = std::fs::metadata(path) {
            self.settled.insert(path.to_owned(), Fingerprint::of(&meta));
        }
    }

    fn process(&mut self, path: &Path) {
        let (geo_attempts, lock_attempts) =
            self.deferred.get(path).map_or((0, 0), |d| (d.geo_attempts, d.lock_attempts));
        let strict_geo = geo_attempts + 1 < GEO_ATTEMPTS;

        let result = (|| -> anyhow::Result<Option<PathBuf>> {
            let job = match prepare(path, self.geo, strict_geo)? {
                Prepared::Skip(why) => {
                    info!("left alone  {}: {why}", self.rel(path));
                    return Ok(None);
                }
                Prepared::Stamp(job) => job,
            };
            let dest = path.with_file_name(formats::output_name(path, job.kind));
            if job.kind == Kind::Raw && dest.exists() {
                info!("left alone  {}: {} already exists", self.rel(path), self.rel(&dest));
                return Ok(None);
            }
            let before = std::fs::metadata(path)?;
            write_stamped(path, &job, &mut self.renderer, &dest, self.config.quality)?;
            if job.kind != Kind::Raw {
                keep_times(&dest, &before).context("restoring file times")?;
            }
            info!("stamped     {}: {}", self.rel(&dest), job.lines.join(" | "));
            Ok((dest != path).then_some(dest))
        })();

        match result {
            Ok(written) => {
                self.settle(path);
                if let Some(extra) = written {
                    self.settle(&extra);
                }
            }
            Err(e) if e.downcast_ref::<GeoUnavailable>().is_some() => {
                let delay = GEO_RETRY_BASE * 2u32.pow(geo_attempts);
                info!("waiting     {}: {e:#}; retrying in {delay:?}", self.rel(path));
                self.deferred.insert(
                    path.to_owned(),
                    Deferred {
                        geo_attempts: geo_attempts + 1,
                        lock_attempts,
                        next_try: Instant::now() + delay,
                    },
                );
            }
            Err(e) if is_locked(&e) && lock_attempts + 1 < LOCKED_ATTEMPTS => {
                info!(
                    "busy        {}: in use by another program; retrying in {LOCKED_RETRY:?}",
                    self.rel(path)
                );
                self.deferred.insert(
                    path.to_owned(),
                    Deferred {
                        geo_attempts,
                        lock_attempts: lock_attempts + 1,
                        next_try: Instant::now() + LOCKED_RETRY,
                    },
                );
            }
            Err(e) => {
                warn!("couldn't stamp {}: {e:#}", self.rel(path));
                self.settle(path);
            }
        }
    }
}

fn skipped_dir(e: &DirEntry) -> bool {
    let name = e.file_name().to_string_lossy();
    e.depth() > 0
        && e.file_type().is_dir()
        && (name.starts_with('.') || SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)))
}

/// A sharing violation / lock / permission problem that may clear up on its own.
fn is_locked(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|io| {
        // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33): another program has it open.
        io.kind() == std::io::ErrorKind::PermissionDenied
            || (cfg!(windows) && matches!(io.raw_os_error(), Some(32 | 33)))
    })
}

/// Give the replaced photo back its original created/modified/accessed times, so photo apps
/// that sort by date aren't disturbed.
fn keep_times(path: &Path, before: &Metadata) -> std::io::Result<()> {
    let mut times = FileTimes::new();
    if let Ok(t) = before.modified() {
        times = times.set_modified(t);
    }
    if let Ok(t) = before.accessed() {
        times = times.set_accessed(t);
    }
    #[cfg(windows)]
    if let Ok(t) = before.created() {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(t);
    }
    std::fs::File::options().write(true).open(path)?.set_times(times)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::tests::{EIFFEL_TAGS, FailingGeocoder, FixedPlace, make_photo, temp_dir};

    fn config(state_file: Option<PathBuf>) -> Config {
        Config {
            settle: Duration::ZERO,
            poll_interval: Duration::from_secs(1),
            events: false,
            quality: 90,
            state_file,
        }
    }

    fn stamped(path: &Path) -> bool {
        crate::metadata::read(path).unwrap().already_stamped
    }

    #[test]
    fn stamps_new_photos_in_place_recursively_and_leaves_old_ones() {
        let root = temp_dir("inplace");
        make_photo(&root.join("old.jpg"), EIFFEL_TAGS);
        // Make sure "old" is strictly older than the watcher's start.
        std::thread::sleep(Duration::from_millis(1100));
        let mut w = InPlace::new(&root, config(None), &FixedPlace).unwrap();
        std::thread::sleep(Duration::from_millis(1100));

        std::fs::create_dir_all(root.join("trip/day 1")).unwrap();
        make_photo(&root.join("trip/day 1/a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("b.png"), EIFFEL_TAGS);
        make_photo(&root.join("bare.jpg"), &[]);
        std::fs::create_dir_all(root.join(SUPPORT_DIR)).unwrap();
        make_photo(&root.join(SUPPORT_DIR).join("helper.jpg"), EIFFEL_TAGS);
        let before = std::fs::metadata(root.join("b.png")).unwrap();
        let bare_before = std::fs::read(root.join("bare.jpg")).unwrap();

        w.scan_once();

        assert!(stamped(&root.join("trip/day 1/a.jpg")), "new photo in a subfolder");
        assert!(stamped(&root.join("b.png")));
        assert!(!stamped(&root.join("old.jpg")), "photos already there are left alone");
        assert!(!stamped(&root.join(SUPPORT_DIR).join("helper.jpg")), "support folder is ignored");
        assert_eq!(std::fs::read(root.join("bare.jpg")).unwrap(), bare_before, "no metadata: untouched");
        // Same name, same modified time, no stray files.
        let after = std::fs::metadata(root.join("b.png")).unwrap();
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(names.is_empty(), "temp files left behind: {names:?}");

        // A second scan does nothing: no re-stamping, and settled files aren't re-read.
        let a = std::fs::read(root.join("trip/day 1/a.jpg")).unwrap();
        w.scan_once();
        assert_eq!(std::fs::read(root.join("trip/day 1/a.jpg")).unwrap(), a);
        assert!(w.pending.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn heartbeat_catches_photos_added_while_not_running() {
        let root = temp_dir("inplace-heartbeat");
        let state = root.join(".state/state.json");
        drop(InPlace::new(&root, config(Some(state.clone())), &FixedPlace).unwrap());
        std::thread::sleep(Duration::from_millis(1100));
        // Arrives while the watcher isn't running…
        make_photo(&root.join("while-off.jpg"), EIFFEL_TAGS);
        std::thread::sleep(Duration::from_millis(1100));
        // …and is still stamped when it starts again.
        let mut w = InPlace::new(&root, config(Some(state)), &FixedPlace).unwrap();
        w.scan_once();
        assert!(stamped(&root.join("while-off.jpg")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn geo_outage_defers() {
        let root = temp_dir("inplace-defer");
        let mut w = InPlace::new(&root, config(None), &FailingGeocoder).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        w.scan_once();
        assert!(!stamped(&root.join("a.jpg")));
        assert_eq!(w.deferred[&root.join("a.jpg")].geo_attempts, 1);
        // Out of retries: stamp with coordinates rather than wait forever.
        let d = w.deferred.get_mut(&root.join("a.jpg")).unwrap();
        d.geo_attempts = GEO_ATTEMPTS;
        d.next_try = Instant::now();
        w.scan_once();
        assert!(stamped(&root.join("a.jpg")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stop_ends_the_loop() {
        let root = temp_dir("inplace-stop");
        let w = InPlace::new(&root, config(None), &FixedPlace).unwrap();
        let waker = w.waker();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            waker.send(Wake::Stop).unwrap();
        });
        let start = Instant::now();
        w.run().unwrap();
        t.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn locked_errors_are_recognized() {
        if cfg!(windows) {
            let e = anyhow::Error::from(std::io::Error::from_raw_os_error(32)).context("renaming");
            assert!(is_locked(&e), "sharing violation, wrapped in context");
        }
        let denied = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(is_locked(&denied));
        assert!(!is_locked(&anyhow::anyhow!("decoding failed")));
    }
}
