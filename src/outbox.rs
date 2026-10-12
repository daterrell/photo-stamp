//! Drop-folder mode: watch a folder tree and move every photo dropped into it to an outbox folder
//! with the same subfolders, stamped. `trip/day 1/a.jpg` becomes `outbox/trip/day 1/a.jpg`.
//!
//! Every photo found is handled, however old its file dates: whether a photo is already stamped
//! is decided by the marker in its metadata alone. Photos that are already stamped, or that have
//! no date or location to stamp, are moved across unchanged. Camera RAW files are never modified;
//! they're moved with a stamped `<name>.jpg` beside them. Other files are left where they are.
//!
//! As in the inbox watcher, filesystem events give instant pickup and polling is the backup; a
//! file is only processed once it has stopped changing.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{FileTimes, Metadata};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use log::{info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::{DirEntry, WalkDir};

use crate::formats::{self, Kind};
use crate::geo::Geocoder;
use crate::render::Renderer;
use crate::stamp::{GeoUnavailable, Prepared, prepare, write_stamped};
use crate::watch::is_temporary;

/// Folder beside the exe holding bundled helpers (exiftool, place data); never scanned.
pub const SUPPORT_DIR: &str = "photo-stamp-files";
/// Default outbox, inside the watched folder; never scanned.
pub const OUTBOX_DIR: &str = "outbox";
const SKIP_DIRS: &[&str] = &[SUPPORT_DIR, "$RECYCLE.BIN", "System Volume Information"];

/// Place lookups are retried this many times (with growing delays) before settling for less.
const GEO_ATTEMPTS: u32 = 4;
const GEO_RETRY_BASE: Duration = Duration::from_secs(60);
/// A file another program has open is retried this often, this many times.
const LOCKED_RETRY: Duration = Duration::from_secs(30);
const LOCKED_ATTEMPTS: u32 = 20;

#[derive(Debug, Clone)]
pub struct Config {
    /// Where finished photos go, mirroring the watched folder's subfolders.
    pub outbox: PathBuf,
    /// How long a file must stay unchanged before it's processed.
    pub settle: Duration,
    /// How often to poll the tree, as a backup to filesystem events.
    pub poll_interval: Duration,
    /// Listen for filesystem events (polling always runs too).
    pub events: bool,
    pub quality: u8,
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

pub struct Outbox<'a> {
    root: PathBuf,
    config: Config,
    geo: &'a dyn Geocoder,
    renderer: Renderer,
    pending: HashMap<PathBuf, Pending>,
    /// Files that couldn't be moved (e.g. undecodable), as we last saw them; retried if they change.
    settled: HashMap<PathBuf, Fingerprint>,
    deferred: HashMap<PathBuf, Deferred>,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
}

impl<'a> Outbox<'a> {
    pub fn new(root: &Path, config: Config, geo: &'a dyn Geocoder) -> anyhow::Result<Self> {
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        std::fs::create_dir_all(&config.outbox)
            .with_context(|| format!("creating {}", config.outbox.display()))?;
        let (wake_tx, wake_rx) = mpsc::channel();
        Ok(Self {
            root: root.to_owned(),
            config,
            geo,
            renderer: Renderer::new(),
            pending: HashMap::new(),
            settled: HashMap::new(),
            deferred: HashMap::new(),
            wake_tx,
            wake_rx,
        })
    }

    /// Something that can wake the watcher, e.g. to stop it from another thread.
    pub fn waker(&self) -> mpsc::Sender<Wake> {
        self.wake_tx.clone()
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
             photos are stamped and moved to {}",
            self.root.display(),
            if _watcher.is_some() { "on" } else { "off" },
            self.config.poll_interval,
            self.config.outbox.display(),
        );

        loop {
            self.scan_once();
            let wait = if self.pending.is_empty() && self.deferred.is_empty() {
                self.config.poll_interval
            } else {
                self.config.settle.min(self.config.poll_interval).max(Duration::from_millis(250))
            };
            match self.wake_rx.recv_timeout(wait) {
                Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
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

    fn skipped_dir(&self, e: &DirEntry) -> bool {
        let name = e.file_name().to_string_lossy();
        e.depth() > 0
            && e.file_type().is_dir()
            && (name.starts_with('.')
                || SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s))
                || same_path(e.path(), &self.config.outbox))
    }

    /// Look through the tree once and process every photo that has settled.
    pub fn scan_once(&mut self) {
        let now = Instant::now();
        let mut present: Vec<PathBuf> = Vec::new();
        let walker = WalkDir::new(&self.root).min_depth(1).into_iter().filter_entry(|e| !self.skipped_dir(e));
        let mut found = Vec::new();
        for entry in walker.filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_type().is_file() || is_temporary(path) || formats::classify(path).is_none() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            found.push((path.to_owned(), Fingerprint::of(&meta)));
        }
        for (path, fp) in found {
            if self.settled.get(&path) == Some(&fp) {
                continue;
            }
            present.push(path.clone());
            let p = self.pending.entry(path).or_insert(Pending { fp, since: now });
            if p.fp != fp {
                *p = Pending { fp, since: now };
            }
        }
        self.pending.retain(|p, _| present.contains(p));
        self.deferred.retain(|p, _| present.contains(p));
        self.settled.retain(|p, _| p.exists());

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

    fn rel<'p>(&self, path: &'p Path) -> &'p Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    fn show(&self, path: &Path) -> String {
        self.rel(path).to_string_lossy().into_owned()
    }

    /// Couldn't move `path`: leave it be until it changes.
    fn settle(&mut self, path: &Path) {
        self.pending.remove(path);
        self.deferred.remove(path);
        if let Ok(meta) = std::fs::metadata(path) {
            self.settled.insert(path.to_owned(), Fingerprint::of(&meta));
        }
    }

    fn done(&mut self, path: &Path) {
        self.pending.remove(path);
        self.deferred.remove(path);
        self.settled.remove(path);
    }

    /// Stamp `path` into the outbox (or move it there unchanged) and remove it from here.
    fn deliver(&mut self, path: &Path, strict_geo: bool) -> anyhow::Result<()> {
        let rel = self.rel(path).to_owned();
        let dir = self.config.outbox.join(rel.parent().unwrap_or(Path::new("")));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = path.file_name().context("no file name")?.to_owned();

        let job = match prepare(path, self.geo, strict_geo)? {
            Prepared::Skip(why) => {
                let dest = free_name(&dir, &name, None);
                move_file(path, &dest)?;
                info!("moved       {} (unstamped: {why})", self.show_out(&dest));
                return Ok(());
            }
            Prepared::Stamp(job) => job,
        };

        let before = std::fs::metadata(path)?;
        if job.kind == Kind::Raw {
            // The untouched RAW and its stamped JPEG travel together under matching names.
            let raw = free_name(&dir, &name, Some(".jpg"));
            let jpg = dir.join(formats::output_name(&raw, job.kind));
            write_stamped(path, &job, &mut self.renderer, &jpg, self.config.quality)?;
            if let Err(e) = move_file(path, &raw) {
                let _ = std::fs::remove_file(&jpg);
                return Err(e);
            }
            info!("stamped     {}: {}", self.show_out(&jpg), job.lines.join(" | "));
        } else {
            let dest = free_name(&dir, &name, None);
            write_stamped(path, &job, &mut self.renderer, &dest, self.config.quality)?;
            // Only once the original is gone is the photo delivered; otherwise try again later.
            if let Err(e) = std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
            {
                let _ = std::fs::remove_file(&dest);
                return Err(e);
            }
            if let Err(e) = keep_times(&dest, &before) {
                warn!("couldn't keep the file dates of {}: {e}", self.show_out(&dest));
            }
            info!("stamped     {}: {}", self.show_out(&dest), job.lines.join(" | "));
        }
        Ok(())
    }

    fn show_out(&self, path: &Path) -> String {
        let base = self.config.outbox.parent().unwrap_or(&self.config.outbox);
        path.strip_prefix(base).unwrap_or(path).to_string_lossy().into_owned()
    }

    fn process(&mut self, path: &Path) {
        let (geo_attempts, lock_attempts) =
            self.deferred.get(path).map_or((0, 0), |d| (d.geo_attempts, d.lock_attempts));
        let strict_geo = geo_attempts + 1 < GEO_ATTEMPTS;

        match self.deliver(path, strict_geo) {
            Ok(()) => self.done(path),
            Err(e) if e.downcast_ref::<GeoUnavailable>().is_some() => {
                let delay = GEO_RETRY_BASE * 2u32.pow(geo_attempts);
                info!("waiting     {}: {e:#}; retrying in {delay:?}", self.show(path));
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
                    self.show(path)
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
                warn!("couldn't stamp {}: {e:#}; leaving it here", self.show(path));
                self.settle(path);
            }
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    norm(a) == norm(b)
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`… if taken. With `companion`, the name with that suffix
/// appended (a RAW's stamped `.jpg`) must be free too.
fn free_name(dir: &Path, name: &std::ffi::OsStr, companion: Option<&str>) -> PathBuf {
    let taken = |p: &Path| {
        p.exists()
            || companion.is_some_and(|c| {
                let mut n = p.as_os_str().to_owned();
                n.push(c);
                Path::new(&n).exists()
            })
    };
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().unwrap_or(name).to_string_lossy();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(OsString::from(format!("{stem} ({n}){ext}"))))
        .find(|c| !taken(c))
        .expect("some name is free")
}

/// Move a file, falling back to copy + delete when a rename can't (another drive).
fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    if let Err(e) = std::fs::remove_file(from) {
        let _ = std::fs::remove_file(to);
        return Err(e).with_context(|| format!("removing {}", from.display()));
    }
    Ok(())
}

/// A sharing violation / lock / permission problem that may clear up on its own.
fn is_locked(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|io| {
        // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33): another program has it open.
        io.kind() == std::io::ErrorKind::PermissionDenied
            || (cfg!(windows) && matches!(io.raw_os_error(), Some(32 | 33)))
    })
}

/// Give the stamped photo its original's created/modified/accessed times, so photo apps that sort
/// by date aren't disturbed.
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

    fn config(root: &Path) -> Config {
        Config {
            outbox: root.join(OUTBOX_DIR),
            settle: Duration::ZERO,
            poll_interval: Duration::from_secs(1),
            events: false,
            quality: 90,
        }
    }

    fn stamped(path: &Path) -> bool {
        crate::metadata::read(path).unwrap().already_stamped
    }

    fn set_old_times(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(400 * 86400);
        let times = FileTimes::new().set_modified(old).set_accessed(old);
        std::fs::File::options().write(true).open(path).unwrap().set_times(times).unwrap();
    }

    #[test]
    fn moves_photos_to_outbox_mirroring_folders_whatever_their_dates() {
        let root = temp_dir("outbox");
        // Already in the folder before the watcher starts, with old dates: still handled.
        make_photo(&root.join("old.jpg"), EIFFEL_TAGS);
        set_old_times(&root.join("old.jpg"));
        let mut w = Outbox::new(&root, config(&root), &FixedPlace).unwrap();

        std::fs::create_dir_all(root.join("trip/day 1")).unwrap();
        make_photo(&root.join("trip/day 1/a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("b.png"), EIFFEL_TAGS);
        make_photo(&root.join("bare.jpg"), &[]);
        std::fs::write(root.join("notes.txt"), "hi").unwrap();
        std::fs::create_dir_all(root.join(SUPPORT_DIR)).unwrap();
        make_photo(&root.join(SUPPORT_DIR).join("helper.jpg"), EIFFEL_TAGS);
        let before = std::fs::metadata(root.join("b.png")).unwrap();
        let bare_before = std::fs::read(root.join("bare.jpg")).unwrap();

        w.scan_once();

        let out = root.join(OUTBOX_DIR);
        assert!(stamped(&out.join("trip/day 1/a.jpg")), "subfolders are mirrored");
        assert!(stamped(&out.join("old.jpg")), "file dates don't matter");
        assert!(stamped(&out.join("b.png")));
        assert_eq!(std::fs::read(out.join("bare.jpg")).unwrap(), bare_before, "no metadata: moved unchanged");
        for gone in ["trip/day 1/a.jpg", "old.jpg", "b.png", "bare.jpg"] {
            assert!(!root.join(gone).exists(), "{gone} should have moved");
        }
        assert!(root.join("trip/day 1").is_dir(), "the folders themselves stay");
        assert!(root.join("notes.txt").exists(), "non-photos are left alone");
        assert!(!stamped(&root.join(SUPPORT_DIR).join("helper.jpg")), "support folder is ignored");
        let after = std::fs::metadata(out.join("b.png")).unwrap();
        assert_eq!(after.modified().unwrap(), before.modified().unwrap(), "modified date kept");
        let stray: Vec<_> = WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".photo-stamp"))
            .collect();
        assert!(stray.is_empty(), "temp files left behind: {stray:?}");

        // The outbox isn't scanned: nothing is stamped twice or moved again.
        let a = std::fs::read(out.join("trip/day 1/a.jpg")).unwrap();
        w.scan_once();
        assert_eq!(std::fs::read(out.join("trip/day 1/a.jpg")).unwrap(), a);
        assert!(w.pending.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn already_stamped_photos_move_unchanged_and_names_never_clash() {
        let root = temp_dir("outbox-clash");
        let mut w = Outbox::new(&root, config(&root), &FixedPlace).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        w.scan_once();
        // The stamped photo comes back in (e.g. copied from the outbox), plus a new one by that name.
        std::fs::create_dir_all(root.join("again")).unwrap();
        std::fs::copy(root.join("outbox/a.jpg"), root.join("again/a.jpg")).unwrap();
        let stamped_bytes = std::fs::read(root.join("again/a.jpg")).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        w.scan_once();

        assert_eq!(
            std::fs::read(root.join("outbox/again/a.jpg")).unwrap(),
            stamped_bytes,
            "not stamped twice"
        );
        assert!(stamped(&root.join("outbox/a.jpg")));
        assert!(stamped(&root.join("outbox/a (2).jpg")), "second a.jpg gets its own name");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn custom_outbox_outside_the_folder() {
        let root = temp_dir("outbox-custom");
        let elsewhere = temp_dir("outbox-custom-dest");
        let mut cfg = config(&root);
        cfg.outbox = elsewhere.join("done");
        let mut w = Outbox::new(&root, cfg, &FixedPlace).unwrap();
        std::fs::create_dir_all(root.join("x")).unwrap();
        make_photo(&root.join("x/a.jpg"), EIFFEL_TAGS);
        w.scan_once();
        assert!(stamped(&elsewhere.join("done/x/a.jpg")));
        assert!(!root.join(OUTBOX_DIR).exists());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn free_name_counts_up_and_respects_companions() {
        let dir = temp_dir("outbox-names");
        std::fs::write(dir.join("a.jpg"), "").unwrap();
        assert_eq!(free_name(&dir, "a.jpg".as_ref(), None), dir.join("a (2).jpg"));
        assert_eq!(free_name(&dir, "b.jpg".as_ref(), None), dir.join("b.jpg"));
        // A RAW whose stamped JPEG name is taken moves under a new name, so the pair still matches.
        std::fs::write(dir.join("c.dng.jpg"), "").unwrap();
        assert_eq!(free_name(&dir, "c.dng".as_ref(), Some(".jpg")), dir.join("c (2).dng"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn geo_outage_defers() {
        let root = temp_dir("outbox-defer");
        let mut w = Outbox::new(&root, config(&root), &FailingGeocoder).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        w.scan_once();
        assert!(root.join("a.jpg").exists(), "waits in place");
        assert!(!root.join("outbox/a.jpg").exists());
        assert_eq!(w.deferred[&root.join("a.jpg")].geo_attempts, 1);
        // Out of retries: stamp with coordinates rather than wait forever.
        let d = w.deferred.get_mut(&root.join("a.jpg")).unwrap();
        d.geo_attempts = GEO_ATTEMPTS;
        d.next_try = Instant::now();
        w.scan_once();
        assert!(stamped(&root.join("outbox/a.jpg")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stop_ends_the_loop() {
        let root = temp_dir("outbox-stop");
        let w = Outbox::new(&root, config(&root), &FixedPlace).unwrap();
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
