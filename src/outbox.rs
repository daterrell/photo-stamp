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
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use log::{info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::{DirEntry, WalkDir};

use crate::formats::{self, Kind};
use crate::geo::Geocoder;
use crate::render::Renderer;
use crate::stamp::{GeoUnavailable, Picture, Prepared, prepare, write_stamped_with};
use crate::status::{self, Entry, Outcome, Queued, Stage, Status, Waiting, Which};
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

/// Quality, settle time and polling come from the live settings in [`Status`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Where finished photos go, mirroring the watched folder's subfolders.
    pub outbox: PathBuf,
    /// Listen for filesystem events (polling always runs too).
    pub events: bool,
}

/// Why the watcher woke up.
pub enum Wake {
    /// Something changed on disk, or a setting changed (e.g. resumed).
    Fs,
    /// Try this photo (a path within the watched folder) again, e.g. after it failed.
    Retry(PathBuf),
    Stop,
}

/// How a photo was delivered to the outbox.
struct Delivered {
    outcome: Outcome,
    reason: Option<String>,
    caption: Vec<String>,
    output: PathBuf,
    thumbs: bool,
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
    status: Arc<Status>,
    /// A stop arrived (seen between photos); the loop ends after the photo in hand.
    stopping: bool,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
}

impl<'a> Outbox<'a> {
    pub fn new(
        root: &Path,
        config: Config,
        geo: &'a dyn Geocoder,
        status: Arc<Status>,
    ) -> anyhow::Result<Self> {
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
            status,
            stopping: false,
            wake_tx,
            wake_rx,
        })
    }

    fn settle_time(&self) -> Duration {
        Duration::from_secs(self.status.settings().settle_secs)
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.status.settings().poll_secs.max(1))
    }

    /// Forget that `rel` failed, so it's tried again on the next scan.
    fn retry(&mut self, rel: &Path) {
        let path = self.root.join(rel);
        self.settled.remove(&path);
        if let Some(d) = self.deferred.get_mut(&path) {
            d.next_try = Instant::now();
        }
        info!("retrying    {}", rel.display());
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
            self.poll_interval(),
            self.config.outbox.display(),
        );
        if self.status.is_paused() {
            info!("paused: new photos are noticed but not processed until resumed");
        }

        loop {
            self.scan_once();
            if self.stopping {
                info!("stopped");
                return Ok(());
            }
            let (settle, poll) = (self.settle_time(), self.poll_interval());
            let wait = if self.pending.is_empty() && self.deferred.is_empty() {
                poll
            } else {
                settle.min(poll).max(Duration::from_millis(250))
            };
            let mut wake = self.wake_rx.recv_timeout(wait);
            if matches!(wake, Ok(Wake::Fs)) {
                // Let a burst of events (a big copy) coalesce before scanning.
                std::thread::sleep(Duration::from_millis(250));
            }
            // Handle everything that's arrived; a stop wins.
            loop {
                match wake {
                    Ok(Wake::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        info!("stopped");
                        return Ok(());
                    }
                    Ok(Wake::Retry(rel)) => self.retry(&rel),
                    Ok(Wake::Fs) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                match self.wake_rx.try_recv() {
                    Ok(w) => wake = Ok(w),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => wake = Err(mpsc::RecvTimeoutError::Disconnected),
                }
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
        self.status.set_health(self.geo.health());
        self.report_queue();
        if self.status.is_paused() {
            return;
        }

        let settle = self.settle_time();
        let mut ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(p, s)| {
                now.duration_since(s.since) >= settle
                    && self.deferred.get(*p).is_none_or(|d| now >= d.next_try)
            })
            .map(|(p, _)| p.clone())
            .collect();
        ready.sort();
        for path in ready {
            // Paused or stopped part-way through a batch.
            if self.status.is_paused() || self.take_wakes() {
                break;
            }
            self.process(&path);
            // Map data may have been downloaded, indexes built.
            self.status.set_health(self.geo.health());
        }
        self.report_queue();
    }

    /// Handle requests that arrived while processing (retries); returns whether to stop.
    fn take_wakes(&mut self) -> bool {
        loop {
            match self.wake_rx.try_recv() {
                Ok(Wake::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.stopping = true;
                    return true;
                }
                Ok(Wake::Retry(rel)) => self.retry(&rel),
                Ok(Wake::Fs) => {}
                Err(mpsc::TryRecvError::Empty) => return false,
            }
        }
    }

    /// Tell the dashboard what's waiting, and why.
    fn report_queue(&self) {
        let now = Instant::now();
        let settle = self.settle_time();
        let paused = self.status.is_paused();
        let until = |t: Instant| status::now_ms() + t.saturating_duration_since(now).as_millis() as u64;
        let mut queue: Vec<Queued> = self
            .pending
            .iter()
            .map(|(p, s)| {
                let waiting = match self.deferred.get(p) {
                    Some(d) if now < d.next_try && d.lock_attempts > 0 => {
                        Waiting::Busy { until: until(d.next_try) }
                    }
                    Some(d) if now < d.next_try => Waiting::Offline { until: until(d.next_try) },
                    _ if now.duration_since(s.since) < settle => Waiting::Settling,
                    _ if paused => Waiting::Paused,
                    _ => Waiting::Ready,
                };
                Queued { path: self.show(p), waiting }
            })
            .collect();
        queue.sort_by(|a, b| a.path.cmp(&b.path));
        self.status.set_queue(queue);
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
    fn deliver(&mut self, path: &Path, id: u64, strict_geo: bool) -> anyhow::Result<Delivered> {
        let rel = self.rel(path).to_owned();
        let dir = self.config.outbox.join(rel.parent().unwrap_or(Path::new("")));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = path.file_name().context("no file name")?.to_owned();

        let job = match prepare(path, self.geo, strict_geo)? {
            Prepared::Skip(why) => {
                self.status.stage(Stage::Moving);
                let dest = free_name(&dir, &name, None);
                move_file(path, &dest)?;
                info!("moved       {} (unstamped: {why})", self.show_out(&dest));
                return Ok(Delivered {
                    outcome: Outcome::Unchanged,
                    reason: Some(why.to_string()),
                    caption: Vec::new(),
                    output: dest,
                    thumbs: false,
                });
            }
            Prepared::Stamp(job) => job,
        };

        self.status.stage(Stage::Drawing);
        let status = self.status.clone();
        let mut on_image = |picture: Picture, img: &image::DynamicImage| match picture {
            Picture::Original => status.save_thumb(id, Which::Before, img),
            Picture::Stamped => {
                status.save_thumb(id, Which::After, img);
                status.stage(Stage::Saving);
            }
        };
        let quality = self.status.settings().quality;
        let before = std::fs::metadata(path)?;
        let output = if job.kind == Kind::Raw {
            // The untouched RAW and its stamped JPEG travel together under matching names.
            let raw = free_name(&dir, &name, Some(".jpg"));
            let jpg = dir.join(formats::output_name(&raw, job.kind));
            write_stamped_with(path, &job, &mut self.renderer, &jpg, quality, &mut on_image)?;
            if let Err(e) = move_file(path, &raw) {
                let _ = std::fs::remove_file(&jpg);
                return Err(e);
            }
            jpg
        } else {
            let dest = free_name(&dir, &name, None);
            write_stamped_with(path, &job, &mut self.renderer, &dest, quality, &mut on_image)?;
            // Only once the original is gone is the photo delivered; otherwise try again later.
            if let Err(e) = std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
            {
                let _ = std::fs::remove_file(&dest);
                return Err(e);
            }
            if let Err(e) = keep_times(&dest, &before) {
                warn!("couldn't keep the file dates of {}: {e}", self.show_out(&dest));
            }
            dest
        };
        info!("stamped     {}: {}", self.show_out(&output), job.lines.join(" | "));
        Ok(Delivered { outcome: Outcome::Stamped, reason: None, caption: job.lines, output, thumbs: true })
    }

    fn downloaded(&self) -> u64 {
        self.geo.health().overture.map_or(0, |o| o.downloaded_bytes)
    }

    fn show_out(&self, path: &Path) -> String {
        let base = self.config.outbox.parent().unwrap_or(&self.config.outbox);
        path.strip_prefix(base).unwrap_or(path).to_string_lossy().into_owned()
    }

    fn process(&mut self, path: &Path) {
        let (geo_attempts, lock_attempts) =
            self.deferred.get(path).map_or((0, 0), |d| (d.geo_attempts, d.lock_attempts));
        let strict_geo = geo_attempts + 1 < GEO_ATTEMPTS;

        let rel = self.show(path);
        let id = self.status.start(&rel);
        let downloaded_before = self.downloaded();
        let result = self.deliver(path, id, strict_geo);
        let downloaded = self.downloaded().saturating_sub(downloaded_before);
        let entry = |outcome, reason, caption, output: Option<String>, thumbs| Entry {
            id,
            path: rel.clone(),
            outcome,
            reason,
            caption,
            output,
            ms: 0,
            downloaded,
            thumbs,
            finished: 0,
        };
        match result {
            Ok(d) => {
                self.done(path);
                let output = Some(self.show_out(&d.output));
                self.status.finish(entry(d.outcome, d.reason, d.caption, output, d.thumbs));
                return;
            }
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
                self.status.finish(entry(Outcome::Failed, Some(format!("{e:#}")), Vec::new(), None, false));
                return;
            }
        }
        // Back in the queue, to try again later.
        let waiting = match self.deferred.get(path) {
            Some(d) if d.lock_attempts > lock_attempts => {
                Waiting::Busy { until: status::now_ms() + LOCKED_RETRY.as_millis() as u64 }
            }
            Some(d) => Waiting::Offline {
                until: status::now_ms()
                    + d.next_try.saturating_duration_since(Instant::now()).as_millis() as u64,
            },
            None => Waiting::Ready,
        };
        self.status.deferred(Queued { path: rel, waiting });
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
    use crate::settings::Settings;
    use crate::stamp::tests::{EIFFEL_TAGS, FailingGeocoder, FixedPlace, make_photo, temp_dir};

    fn config(root: &Path) -> Config {
        Config { outbox: root.join(OUTBOX_DIR), events: false }
    }

    /// In-memory status (no history or previews on disk), processing as soon as files are seen.
    fn status(root: &Path) -> Arc<Status> {
        let settings = Settings { settle_secs: 0, poll_secs: 1, quality: 90, ..Settings::default() };
        Arc::new(Status::new(root, &root.join(OUTBOX_DIR), None, "test", settings))
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
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status(&root)).unwrap();

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
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status(&root)).unwrap();
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
        let mut w = Outbox::new(&root, cfg, &FixedPlace, status(&root)).unwrap();
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
        let mut w = Outbox::new(&root, config(&root), &FailingGeocoder, status(&root)).unwrap();
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
    fn reports_results_and_honors_pause_and_retry() {
        let root = temp_dir("outbox-status");
        let st = status(&root);
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, st.clone()).unwrap();
        st.set_paused(true);
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        // Good metadata, but a JPEG named .png: it can't be decoded as what it claims to be.
        make_photo(&root.join("broken.jpg"), EIFFEL_TAGS);
        std::fs::rename(root.join("broken.jpg"), root.join("broken.png")).unwrap();
        w.scan_once();
        assert!(root.join("a.jpg").exists(), "paused: noticed but not processed");
        st.snapshot(|s| {
            assert_eq!(s.queue.len(), 2);
            assert!(s.queue.iter().all(|q| q.waiting == Waiting::Paused));
        });

        st.set_paused(false);
        w.scan_once();
        assert!(stamped(&root.join("outbox/a.jpg")));
        st.snapshot(|s| {
            assert!(s.queue.is_empty() && s.current.is_none());
            assert_eq!((s.session.stamped, s.session.failed), (1, 1));
            let a = s.recent.iter().find(|e| e.path == "a.jpg").unwrap();
            assert_eq!(a.outcome, Outcome::Stamped);
            assert_eq!(a.caption[0], "Eiffel Tower (Tour Eiffel)");
            assert_eq!(a.output.as_deref().map(|o| o.replace('\\', "/")), Some("outbox/a.jpg".into()));
            let broken = s.recent.iter().find(|e| e.path == "broken.png").unwrap();
            assert_eq!(broken.outcome, Outcome::Failed);
            assert!(broken.reason.is_some());
        });

        // A failed photo is left alone until it changes, or until retried.
        w.scan_once();
        st.snapshot(|s| assert_eq!(s.session.failed, 1));
        w.retry(Path::new("broken.png"));
        w.scan_once();
        st.snapshot(|s| assert_eq!(s.session.failed, 2, "tried again"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stop_is_noticed_between_photos() {
        let root = temp_dir("outbox-stop-batch");
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status(&root)).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("b.jpg"), EIFFEL_TAGS);
        w.waker().send(Wake::Stop).unwrap();
        w.scan_once();
        assert!(w.stopping);
        assert!(
            root.join("a.jpg").exists() && root.join("b.jpg").exists(),
            "the batch stops before starting"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stop_ends_the_loop() {
        let root = temp_dir("outbox-stop");
        let w = Outbox::new(&root, config(&root), &FixedPlace, status(&root)).unwrap();
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
