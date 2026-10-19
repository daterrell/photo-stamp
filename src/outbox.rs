//! Drop-folder mode: watch a folder tree and move every photo dropped into it to an outbox folder
//! with the same subfolders, stamped. `trip/day 1/a.jpg` becomes `outbox/trip/day 1/a.jpg`.
//!
//! Every photo found is handled, however old its file dates: whether a photo is already stamped
//! is decided by the marker in its metadata alone. Photos that are already stamped, or that have
//! no date or location to stamp, are moved across unchanged. Camera RAW files are never modified;
//! they're moved with a stamped `<name>.jpg` beside them. Other files are left where they are.
//!
//! Places come from the family's place server. While it can't be reached, photos wait in the queue
//! (the dashboard shows the nearest town from the local towns database). After the person's wait
//! time, or when asked to ("stamp now"), a photo is stamped with what's known on this PC instead.
//!
//! The original of every photo that's stamped is kept in `originals/`, with the same subfolders,
//! so it can be stamped again later.
//!
//! As in the inbox watcher, filesystem events give instant pickup and polling is the backup; a
//! file is only processed once it has stopped changing.
//!
//! Several photos are stamped at a time (the `workers` setting): each worker takes a photo through
//! the whole way (metadata, place lookup, drawing, saving, moving), while the watcher hands out
//! photos and keeps the books on one thread.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{FileTimes, Metadata};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use log::{info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use walkdir::{DirEntry, WalkDir};

use crate::formats::{self, Kind};
use crate::geo::{Geocoder, GpsPoint};
use crate::render::Renderer;
use crate::stamp::{GeoUnavailable, Picture, Prepared, prepare, write_stamped_with};
use crate::status::{self, Entry, Outcome, Queued, Stage, Status, Waiting, Which};
use crate::watch::is_temporary;

/// Folder beside the exe holding bundled helpers (exiftool, place data); never scanned.
pub const SUPPORT_DIR: &str = "photo-stamp-files";
/// Default outbox, inside the watched folder; never scanned.
pub const OUTBOX_DIR: &str = "outbox";
/// Default folder for the originals of stamped photos, inside the watched folder; never scanned.
pub const ORIGINALS_DIR: &str = "originals";
const SKIP_DIRS: &[&str] = &[SUPPORT_DIR, "$RECYCLE.BIN", "System Volume Information"];

/// While the place server can't be reached, waiting photos are tried again after this long,
/// doubling up to the maximum.
const GEO_RETRY_BASE: Duration = Duration::from_secs(60);
const GEO_RETRY_MAX: Duration = Duration::from_secs(10 * 60);
/// How often to look for results whose photo has left the outbox (they're dropped from the history).
const FORGET_GONE_EVERY: Duration = Duration::from_secs(60);
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
    /// Where the originals of stamped photos are kept (`None`: they aren't).
    pub originals: Option<PathBuf>,
    /// Where to remember since when photos have been waiting, across restarts.
    pub waiting_file: Option<PathBuf>,
}

/// Why the watcher woke up.
pub enum Wake {
    /// Something changed on disk, or a setting changed (e.g. resumed).
    Fs,
    /// Try this photo (a path within the watched folder) again, e.g. after it failed.
    Retry(PathBuf),
    /// Stop waiting for the place server: stamp this waiting photo (or all of them, `None`) now
    /// with what's known on this PC.
    StampNow(Option<PathBuf>),
    Stop,
}

/// How a photo was delivered to the outbox.
struct Delivered {
    outcome: Outcome,
    reason: Option<String>,
    caption: Vec<String>,
    output: PathBuf,
    thumbs: bool,
    /// Where the original was kept.
    original: Option<PathBuf>,
    gps: Option<GpsPoint>,
    /// Reading the metadata and looking the place up.
    lookup: Duration,
}

/// A photo handed to a worker.
struct Job {
    path: PathBuf,
    id: u64,
    strict_geo: bool,
}

/// A worker's answer for a [`Job`].
struct Done {
    job: Job,
    result: anyhow::Result<Delivered>,
    took: Duration,
}

/// What the workers share: everything a photo needs on its way to the outbox.
struct Ctx<'a> {
    root: PathBuf,
    config: Config,
    geo: &'a (dyn Geocoder + Sync),
    status: Arc<Status>,
    /// Names in the outbox and originals chosen by photos in hand but maybe not written yet, so
    /// two workers can't pick the same one.
    claimed: Mutex<HashSet<PathBuf>>,
}

/// The names one photo has claimed (see [`Ctx::claimed`]); given back when dropped.
struct Claims<'c> {
    all: &'c Mutex<HashSet<PathBuf>>,
    mine: Vec<PathBuf>,
}

impl Claims<'_> {
    /// A free name in `dir` (see [`free_name`]) that no other photo in hand has chosen either.
    fn free_name(&mut self, dir: &Path, name: &std::ffi::OsStr, companion: Option<&str>) -> PathBuf {
        let mut all = self.all.lock().unwrap();
        let path = free_name_where(dir, name, companion, |p| all.contains(p));
        let mut claim = vec![path.clone()];
        if let Some(c) = companion {
            let mut n = path.as_os_str().to_owned();
            n.push(c);
            claim.push(n.into());
        }
        for c in claim {
            all.insert(c.clone());
            self.mine.push(c);
        }
        path
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        let mut all = self.all.lock().unwrap_or_else(|e| e.into_inner());
        for p in &self.mine {
            all.remove(p);
        }
    }
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
    /// Where it was taken, and the nearest town, while it waits for the place server.
    gps: Option<GpsPoint>,
    near: Option<String>,
}

pub struct Outbox<'a> {
    root: PathBuf,
    config: Config,
    geo: &'a (dyn Geocoder + Sync),
    /// One per worker, kept between batches (each holds the loaded fonts).
    renderers: Vec<Renderer>,
    pending: HashMap<PathBuf, Pending>,
    /// Files that couldn't be moved (e.g. undecodable), as we last saw them; retried if they change.
    settled: HashMap<PathBuf, Fingerprint>,
    deferred: HashMap<PathBuf, Deferred>,
    status: Arc<Status>,
    /// A stop arrived (seen between photos); the loop ends after the photo in hand.
    stopping: bool,
    /// Since when (ms since the epoch) each photo has been waiting, by path within the folder.
    first_seen: HashMap<String, u64>,
    /// Photos to stamp now with local data rather than wait for the place server.
    stamp_now: HashSet<PathBuf>,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
    /// When results whose photo left the outbox were last looked for.
    forgot_at: Option<Instant>,
}

impl<'a> Outbox<'a> {
    pub fn new(
        root: &Path,
        config: Config,
        geo: &'a (dyn Geocoder + Sync),
        status: Arc<Status>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
        std::fs::create_dir_all(&config.outbox)
            .with_context(|| format!("creating {}", config.outbox.display()))?;
        if let Some(o) = &config.originals {
            std::fs::create_dir_all(o).with_context(|| format!("creating {}", o.display()))?;
        }
        let first_seen = config
            .waiting_file
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (wake_tx, wake_rx) = mpsc::channel();
        Ok(Self {
            root: root.to_owned(),
            config,
            geo,
            renderers: Vec::new(),
            pending: HashMap::new(),
            settled: HashMap::new(),
            deferred: HashMap::new(),
            status,
            stopping: false,
            first_seen,
            stamp_now: HashSet::new(),
            wake_tx,
            wake_rx,
            forgot_at: None,
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

    /// Stamp a waiting photo (or every waiting photo) now with what's known on this PC.
    fn stamp_now(&mut self, rel: Option<&Path>) {
        let paths: Vec<PathBuf> = match rel {
            Some(r) => vec![self.root.join(r)],
            None => self.pending.keys().cloned().collect(),
        };
        for p in paths {
            if let Some(d) = self.deferred.get_mut(&p) {
                d.next_try = Instant::now();
            }
            self.stamp_now.insert(p);
        }
        info!(
            "stamping now with local place data: {}",
            rel.map_or("every waiting photo".into(), |r| r.display().to_string())
        );
    }

    /// Whether `path` has waited for the place server longer than the person allows.
    fn overdue(&self, path: &Path) -> bool {
        let Some(max) = self.geo.max_wait() else { return false };
        self.first_seen
            .get(&self.show(path))
            .is_some_and(|&t| status::now_ms().saturating_sub(t) >= max.as_millis() as u64)
    }

    fn save_waiting(&self) {
        let Some(p) = &self.config.waiting_file else { return };
        let result = serde_json::to_vec(&self.first_seen).map_err(std::io::Error::other).and_then(|b| {
            let tmp = p.with_extension("tmp");
            std::fs::write(&tmp, b)?;
            std::fs::rename(&tmp, p)
        });
        if let Err(e) = result {
            warn!("couldn't save the waiting list: {e}");
        }
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
                    Ok(Wake::StampNow(rel)) => self.stamp_now(rel.as_deref()),
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
                || same_path(e.path(), &self.config.outbox)
                || self.config.originals.as_deref().is_some_and(|o| same_path(e.path(), o)))
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
        self.stamp_now.retain(|p| present.contains(p));
        // Since when each photo has been waiting (kept across restarts, for the wait time).
        let waiting: HashSet<String> = self.pending.keys().map(|p| self.show(p)).collect();
        let before = self.first_seen.len();
        self.first_seen.retain(|k, _| waiting.contains(k));
        let mut changed = self.first_seen.len() != before;
        for k in waiting {
            if let std::collections::hash_map::Entry::Vacant(e) = self.first_seen.entry(k) {
                e.insert(status::now_ms());
                changed = true;
            }
        }
        if changed {
            self.save_waiting();
        }
        self.status.set_health(self.geo.health());
        self.report_queue();
        if self.forgot_at.is_none_or(|t| t.elapsed() >= FORGET_GONE_EVERY) {
            self.forgot_at = Some(Instant::now());
            let n = self.status.forget_gone();
            if n > 0 {
                info!("{n} photos have left the outbox; they're no longer listed");
            }
        }
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
        if !ready.is_empty() {
            self.process_all(ready);
        }
        self.report_queue();
    }

    /// Stamp `ready` with several workers, handing each a photo as it comes free. Pausing or
    /// stopping part-way lets the photos in hand finish but starts no more.
    fn process_all(&mut self, ready: Vec<PathBuf>) {
        let workers = self.status.settings().worker_count().clamp(1, ready.len());
        self.status.set_workers(workers);
        while self.renderers.len() < workers {
            self.renderers.push(Renderer::new());
        }
        let mut renderers = std::mem::take(&mut self.renderers);
        let ctx = Ctx {
            root: self.root.clone(),
            config: self.config.clone(),
            geo: self.geo,
            status: self.status.clone(),
            claimed: Mutex::new(HashSet::new()),
        };
        let started = Instant::now();
        let (mut count, mut lookups, mut busy) = (0u32, Duration::ZERO, Duration::ZERO);
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let job_rx = Mutex::new(job_rx);
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        std::thread::scope(|scope| {
            for renderer in renderers.iter_mut().take(workers) {
                let (ctx, job_rx, done_tx) = (&ctx, &job_rx, done_tx.clone());
                scope.spawn(move || {
                    loop {
                        let job = job_rx.lock().unwrap().recv();
                        let Ok(job) = job else { return };
                        let t = Instant::now();
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            deliver(ctx, renderer, &job.path, job.id, job.strict_geo)
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("photo-stamp crashed while stamping it")));
                        if done_tx.send(Done { job, result, took: t.elapsed() }).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(done_tx);

            let mut ready = ready.into_iter();
            let mut in_hand = 0;
            loop {
                // Keep every worker busy until the batch runs out, is paused or stopped.
                while in_hand < workers && !self.status.is_paused() && !self.take_wakes() {
                    let Some(path) = ready.next() else { break };
                    // Settled or gone meanwhile (e.g. a retry of something else): skip.
                    if !self.pending.contains_key(&path) {
                        continue;
                    }
                    let job = self.begin(path);
                    if job_tx.send(job).is_err() {
                        break;
                    }
                    in_hand += 1;
                }
                if in_hand == 0 {
                    break;
                }
                let Ok(done) = done_rx.recv() else { break };
                in_hand -= 1;
                count += 1;
                busy += done.took;
                if let Ok(d) = &done.result {
                    lookups += d.lookup;
                }
                self.apply(done.job, done.result);
                // Map data may have been downloaded, indexes built.
                self.status.set_health(self.geo.health());
            }
            // Let the workers go.
            drop(job_tx);
        });
        self.renderers = renderers;
        if count > 1 {
            info!(
                "{count} photos in {:.1} s with {workers} at a time (lookups {:.1} s, stamping {:.1} s of worker time)",
                started.elapsed().as_secs_f64(),
                lookups.as_secs_f64(),
                busy.saturating_sub(lookups).as_secs_f64(),
            );
        }
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
                Ok(Wake::StampNow(rel)) => self.stamp_now(rel.as_deref()),
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
                    Some(d) if now < d.next_try => Waiting::Offline {
                        until: until(d.next_try),
                        near: d.near.clone(),
                        stamp_by: self.stamp_by(p),
                    },
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

    /// When a waiting photo will be stamped with local data if the server still can't be reached.
    fn stamp_by(&self, path: &Path) -> Option<u64> {
        let max = self.geo.max_wait()?;
        Some(self.first_seen.get(&self.show(path))? + max.as_millis() as u64)
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
        if self.first_seen.remove(&self.show(path)).is_some() {
            self.save_waiting();
        }
    }

    fn show_out(&self, path: &Path) -> String {
        show_out(&self.config, path)
    }

    /// Mark `path` as in progress and say how to stamp it.
    fn begin(&mut self, path: PathBuf) -> Job {
        let geo_attempts = self.deferred.get(&path).map_or(0, |d| d.geo_attempts);
        // Wait for the place server unless asked not to, or the photo has waited long enough.
        let forced = self.stamp_now.contains(&path);
        let overdue = self.overdue(&path);
        if overdue && !forced && geo_attempts > 0 {
            info!(
                "waited long enough for the place server: stamping {} with local place data",
                self.show(&path)
            );
        }
        let id = self.status.start(&self.show(&path));
        Job { path, id, strict_geo: !forced && !overdue }
    }

    /// Keep the books for a photo a worker is done with: delivered, back in the queue, or failed.
    fn apply(&mut self, job: Job, result: anyhow::Result<Delivered>) {
        let Job { path, id, .. } = job;
        let path = path.as_path();
        let (geo_attempts, lock_attempts) =
            self.deferred.get(path).map_or((0, 0), |d| (d.geo_attempts, d.lock_attempts));
        let rel = self.show(path);
        let entry = |outcome,
                     reason,
                     caption,
                     output: Option<String>,
                     thumbs,
                     original: Option<String>,
                     gps: Option<GpsPoint>,
                     lookup: Duration| Entry {
            id,
            path: rel.clone(),
            outcome,
            reason,
            caption,
            output,
            ms: 0,
            downloaded: 0,
            thumbs,
            finished: 0,
            original,
            gps: gps.map(|g| [g.lat, g.lon]),
            lookup_ms: lookup.as_millis() as u64,
        };
        match result {
            Ok(d) => {
                self.done(path);
                self.stamp_now.remove(path);
                let output = Some(self.show_out(&d.output));
                let original = d.original.as_deref().map(|o| self.show_out(o));
                self.status.finish(entry(
                    d.outcome, d.reason, d.caption, output, d.thumbs, original, d.gps, d.lookup,
                ));
                return;
            }
            Err(e) if e.downcast_ref::<GeoUnavailable>().is_some() => {
                let at = e.downcast_ref::<GeoUnavailable>().map(|g| g.at);
                let delay = (GEO_RETRY_BASE * 2u32.pow(geo_attempts.min(8))).min(GEO_RETRY_MAX);
                if geo_attempts == 0 {
                    info!("waiting     {}: {e:#}; trying again in {delay:?}", self.show(path));
                }
                let near = self
                    .deferred
                    .get(path)
                    .and_then(|d| d.near.clone())
                    .or_else(|| at.and_then(|a| self.geo.nearby_town(a)));
                self.deferred.insert(
                    path.to_owned(),
                    Deferred {
                        geo_attempts: geo_attempts + 1,
                        lock_attempts,
                        next_try: Instant::now() + delay,
                        gps: at,
                        near,
                    },
                );
            }
            Err(e) if is_locked(&e) && lock_attempts + 1 < LOCKED_ATTEMPTS => {
                info!(
                    "busy        {}: in use by another program; retrying in {LOCKED_RETRY:?}",
                    self.show(path)
                );
                let (gps, near) = self.deferred.get(path).map_or((None, None), |d| (d.gps, d.near.clone()));
                self.deferred.insert(
                    path.to_owned(),
                    Deferred {
                        geo_attempts,
                        lock_attempts: lock_attempts + 1,
                        next_try: Instant::now() + LOCKED_RETRY,
                        gps,
                        near,
                    },
                );
            }
            Err(e) => {
                warn!("couldn't stamp {}: {e:#}; leaving it here", self.show(path));
                self.settle(path);
                self.stamp_now.remove(path);
                self.status.finish(entry(
                    Outcome::Failed,
                    Some(format!("{e:#}")),
                    Vec::new(),
                    None,
                    false,
                    None,
                    None,
                    Duration::ZERO,
                ));
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
                near: d.near.clone(),
                stamp_by: self.stamp_by(path),
            },
            None => Waiting::Ready,
        };
        self.status.deferred(id, Queued { path: rel, waiting });
    }
}

impl Ctx<'_> {
    fn rel<'p>(&self, path: &'p Path) -> &'p Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }

    fn show_out(&self, path: &Path) -> String {
        show_out(&self.config, path)
    }
}

/// `path` (in the outbox or originals) relative to the folder holding the outbox, for messages.
fn show_out(config: &Config, path: &Path) -> String {
    let base = config.outbox.parent().unwrap_or(&config.outbox);
    path.strip_prefix(base).unwrap_or(path).to_string_lossy().into_owned()
}

/// Stamp `path` into the outbox (or move it there unchanged) and remove it from here. Runs on a
/// worker; `renderer` is that worker's own.
fn deliver(
    ctx: &Ctx,
    renderer: &mut Renderer,
    path: &Path,
    id: u64,
    strict_geo: bool,
) -> anyhow::Result<Delivered> {
    let mut names = Claims { all: &ctx.claimed, mine: Vec::new() };
    let rel = ctx.rel(path).to_owned();
    let dir = ctx.config.outbox.join(rel.parent().unwrap_or(Path::new("")));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().context("no file name")?.to_owned();

    let looking = Instant::now();
    let prepared = prepare(path, ctx.geo, strict_geo)?;
    let lookup = looking.elapsed();
    let job = match prepared {
        Prepared::Skip(why) => {
            ctx.status.stage(id, Stage::Moving);
            let dest = names.free_name(&dir, &name, None);
            move_file(path, &dest)?;
            info!("moved       {} (unstamped: {why})", ctx.show_out(&dest));
            return Ok(Delivered {
                outcome: Outcome::Unchanged,
                reason: Some(why.to_string()),
                caption: Vec::new(),
                output: dest,
                thumbs: false,
                original: None,
                gps: None,
                lookup,
            });
        }
        Prepared::Stamp(job) => job,
    };

    ctx.status.stage(id, Stage::Drawing);
    let status = &ctx.status;
    let mut on_image = |picture: Picture, img: &image::DynamicImage| match picture {
        Picture::Original => status.save_thumb(id, Which::Before, img),
        Picture::Stamped => {
            status.save_thumb(id, Which::After, img);
            status.stage(id, Stage::Saving);
        }
    };
    let quality = ctx.status.settings().quality;
    let before = std::fs::metadata(path)?;
    let mut original = None;
    let output = if job.kind == Kind::Raw {
        // The untouched RAW and its stamped JPEG travel together under matching names.
        let raw = names.free_name(&dir, &name, Some(".jpg"));
        let jpg = dir.join(formats::output_name(&raw, job.kind));
        write_stamped_with(path, &job, renderer, &jpg, quality, &mut on_image)?;
        if let Err(e) = move_file(path, &raw) {
            let _ = std::fs::remove_file(&jpg);
            return Err(e);
        }
        jpg
    } else {
        let dest = names.free_name(&dir, &name, None);
        write_stamped_with(path, &job, renderer, &dest, quality, &mut on_image)?;
        // Only once the original is moved aside (or gone) is the photo delivered; otherwise
        // try again later.
        let put_aside = match &ctx.config.originals {
            Some(o) => {
                let odir = o.join(rel.parent().unwrap_or(Path::new("")));
                std::fs::create_dir_all(&odir).map_err(anyhow::Error::from).and_then(|()| {
                    let kept = names.free_name(&odir, &name, None);
                    move_file(path, &kept).map(|()| Some(kept))
                })
            }
            None => std::fs::remove_file(path)
                .map(|()| None)
                .with_context(|| format!("removing {}", path.display())),
        };
        match put_aside {
            Ok(kept) => original = kept,
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                return Err(e);
            }
        }
        if let Err(e) = keep_times(&dest, &before) {
            warn!("couldn't keep the file dates of {}: {e}", ctx.show_out(&dest));
        }
        dest
    };
    info!("stamped     {}: {}", ctx.show_out(&output), job.lines.join(" | "));
    Ok(Delivered {
        outcome: Outcome::Stamped,
        reason: None,
        caption: job.lines,
        output,
        thumbs: true,
        original,
        gps: job.info.gps,
        lookup,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    norm(a) == norm(b)
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`… if taken. With `companion`, the name with that suffix
/// appended (a RAW's stamped `.jpg`) must be free too.
pub fn free_name(dir: &Path, name: &std::ffi::OsStr, companion: Option<&str>) -> PathBuf {
    free_name_where(dir, name, companion, |_| false)
}

/// [`free_name`], also passing over names `claimed` says are taken.
fn free_name_where(
    dir: &Path,
    name: &std::ffi::OsStr,
    companion: Option<&str>,
    claimed: impl Fn(&Path) -> bool,
) -> PathBuf {
    let used = |p: &Path| claimed(p) || p.exists();
    let taken = |p: &Path| {
        used(p)
            || companion.is_some_and(|c| {
                let mut n = p.as_os_str().to_owned();
                n.push(c);
                used(Path::new(&n))
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

/// Stamp a photo the dashboard lists again, with the caption the person wrote, replacing its
/// stamped copy in the outbox. The pixels come from the untouched original: the one kept in
/// `originals`, the RAW beside its JPEG, or (for a photo moved to the outbox unstamped) the photo
/// itself, which is first put aside in `originals`. Returns the result as it now stands.
pub fn restamp(
    status: &Status,
    entry: &Entry,
    lines: Vec<String>,
    originals: &Path,
    renderer: &mut Renderer,
) -> anyhow::Result<Entry> {
    let config = Config {
        outbox: status.outbox().to_owned(),
        events: false,
        originals: Some(originals.to_owned()),
        waiting_file: None,
    };
    let base = config.outbox.parent().unwrap_or(&config.outbox).to_owned();
    let output = base.join(entry.output.as_deref().context("it isn't in the outbox")?);
    anyhow::ensure!(output.is_file(), "it's no longer in the outbox ({})", output.display());
    let raw_of = |jpg: &Path| {
        let raw = PathBuf::from(jpg.as_os_str().to_string_lossy().strip_suffix(".jpg")?);
        (formats::classify(&raw) == Some(Kind::Raw) && raw.is_file()).then_some(raw)
    };
    let mut original = entry.original.clone();
    let (source, dest) = match entry.outcome {
        Outcome::Failed => anyhow::bail!("it couldn't be processed; use Try again instead"),
        Outcome::Stamped => match (&entry.original, raw_of(&output)) {
            (Some(o), _) => {
                let o = base.join(o);
                anyhow::ensure!(o.is_file(), "its original is no longer at {}", o.display());
                (o, output.clone())
            }
            (None, Some(raw)) => (raw, output.clone()),
            (None, None) => anyhow::bail!("its original wasn't kept, so it can't be stamped again"),
        },
        Outcome::Unchanged => {
            anyhow::ensure!(
                !crate::metadata::read(&output)?.already_stamped,
                "it was already stamped when it was dropped here, and its original isn't here"
            );
            if formats::classify(&output) == Some(Kind::Raw) {
                // The RAW stays; its stamped JPEG goes beside it.
                let jpg = output.with_file_name(formats::output_name(&output, Kind::Raw));
                anyhow::ensure!(!jpg.exists(), "{} is already there beside it", jpg.display());
                (output.clone(), jpg)
            } else {
                let rel = Path::new(&entry.path);
                let odir = originals.join(rel.parent().unwrap_or(Path::new("")));
                std::fs::create_dir_all(&odir)?;
                let kept = free_name(&odir, output.file_name().context("no file name")?, None);
                move_file(&output, &kept)?;
                original = Some(show_out(&config, &kept));
                (kept, output.clone())
            }
        }
    };
    let kind = formats::classify(&source).context("not a supported photo format")?;
    let info = crate::metadata::read(&source)?;
    let gps = entry.gps.or(info.gps.map(|g| [g.lat, g.lon]));
    let job = crate::stamp::Job { kind, info, lines };
    let before = std::fs::metadata(&source)?;
    let id = entry.id;
    let mut on_image = |picture: Picture, img: &image::DynamicImage| match picture {
        Picture::Original => status.save_thumb(id, Which::Before, img),
        Picture::Stamped => status.save_thumb(id, Which::After, img),
    };
    write_stamped_with(&source, &job, renderer, &dest, status.settings().quality, &mut on_image)?;
    if let Err(e) = keep_times(&dest, &before) {
        warn!("couldn't keep the file dates of {}: {e}", dest.display());
    }
    info!("restamped   {}: {}", show_out(&config, &dest), job.lines.join(" | "));
    Ok(Entry {
        outcome: Outcome::Stamped,
        reason: None,
        caption: job.lines,
        output: Some(show_out(&config, &dest)),
        thumbs: true,
        original,
        gps,
        ..entry.clone()
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
    use crate::geo::Place;
    use crate::settings::Settings;
    use crate::stamp::{
        self,
        tests::{EIFFEL_TAGS, FixedPlace, OfflineWithTown, make_photo, temp_dir},
    };

    /// The place server can't be reached and the person won't wait at all.
    struct OfflineNoWait;
    impl Geocoder for OfflineNoWait {
        fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
            anyhow::bail!("server unreachable")
        }
        fn local_place(&self, at: GpsPoint) -> Option<Place> {
            OfflineWithTown.local_place(at)
        }
        fn max_wait(&self) -> Option<Duration> {
            Some(Duration::ZERO)
        }
    }

    fn config(root: &Path) -> Config {
        Config {
            outbox: root.join(OUTBOX_DIR),
            events: false,
            originals: Some(root.join(ORIGINALS_DIR)),
            waiting_file: Some(root.join(".waiting.json")),
        }
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

    /// Stamping `n` at a time.
    fn status_with_workers(root: &Path, n: usize) -> Arc<Status> {
        let st = status(root);
        st.set_settings(Settings { workers: n, ..st.settings() });
        st
    }

    /// Answers like [`FixedPlace`] after `delay`, counting how many lookups overlap, and calls
    /// `hook` first (to pause or stop the watcher mid-batch).
    #[derive(Default)]
    struct SlowPlace {
        delay: Duration,
        now: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
        hook: Mutex<Option<Box<dyn Fn() + Send>>>,
    }
    impl Geocoder for SlowPlace {
        fn place(&self, at: GpsPoint) -> anyhow::Result<Option<Place>> {
            use std::sync::atomic::Ordering::SeqCst;
            if let Some(hook) = self.hook.lock().unwrap().as_ref() {
                hook();
            }
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.most.fetch_max(n, SeqCst);
            std::thread::sleep(self.delay);
            self.now.fetch_sub(1, SeqCst);
            FixedPlace.place(at)
        }
    }

    fn count_files(dir: &Path) -> usize {
        WalkDir::new(dir).into_iter().filter_map(Result::ok).filter(|e| e.file_type().is_file()).count()
    }

    #[test]
    fn stamps_a_batch_several_at_a_time() {
        let root = temp_dir("outbox-parallel");
        let st = status_with_workers(&root, 4);
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, st.clone()).unwrap();
        for i in 0..12 {
            let dir = root.join(format!("day {}", i % 3));
            std::fs::create_dir_all(&dir).unwrap();
            make_photo(&dir.join(format!("p{i}.jpg")), EIFFEL_TAGS);
        }
        w.scan_once();
        for i in 0..12 {
            let rel = format!("day {}/p{i}.jpg", i % 3);
            assert!(stamped(&root.join(OUTBOX_DIR).join(&rel)), "{rel}");
            assert!(root.join(ORIGINALS_DIR).join(&rel).exists(), "original of {rel} kept");
            assert!(!root.join(&rel).exists());
        }
        assert_eq!(w.renderers.len(), 4, "a renderer per worker, kept for the next batch");
        st.snapshot(|s| {
            assert!(s.working.is_empty() && s.current.is_none());
            assert_eq!(s.recent.len(), 12);
            let ids: HashSet<u64> = s.recent.iter().map(|e| e.id).collect();
            assert_eq!(ids.len(), 12, "each photo has its own id");
            assert!(s.recent.iter().all(|e| e.outcome == Outcome::Stamped));
        });
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lookups_overlap_but_never_exceed_the_worker_count() {
        let root = temp_dir("outbox-overlap");
        let geo = SlowPlace { delay: Duration::from_millis(300), ..Default::default() };
        let mut w = Outbox::new(&root, config(&root), &geo, status_with_workers(&root, 4)).unwrap();
        for i in 0..8 {
            make_photo(&root.join(format!("p{i}.jpg")), EIFFEL_TAGS);
        }
        let t = Instant::now();
        w.scan_once();
        let took = t.elapsed();
        assert_eq!(count_files(&root.join(OUTBOX_DIR)), 8);
        let most = geo.most.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(most, 4, "four lookups at once, no more");
        // One at a time the lookups alone would take 8 × 300 ms.
        assert!(took < Duration::from_millis(8 * 300), "took {took:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pausing_mid_batch_lets_the_photos_in_hand_finish() {
        let root = temp_dir("outbox-pause");
        let st = status_with_workers(&root, 2);
        let geo = SlowPlace { delay: Duration::from_millis(50), ..Default::default() };
        let pauser = st.clone();
        *geo.hook.lock().unwrap() = Some(Box::new(move || pauser.set_paused(true)));
        let mut w = Outbox::new(&root, config(&root), &geo, st.clone()).unwrap();
        for i in 0..6 {
            make_photo(&root.join(format!("p{i}.jpg")), EIFFEL_TAGS);
        }
        w.scan_once();
        assert_eq!(count_files(&root.join(OUTBOX_DIR)), 2, "the two in hand finish");
        assert_eq!(w.pending.len(), 4, "the rest wait");
        st.snapshot(|s| assert!(s.working.is_empty()));
        // Resumed: the rest go through.
        *geo.hook.lock().unwrap() = None;
        st.set_paused(false);
        w.scan_once();
        assert_eq!(count_files(&root.join(OUTBOX_DIR)), 6);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stopping_mid_batch_finishes_the_photos_in_hand() {
        let root = temp_dir("outbox-stop-mid-batch");
        let geo = SlowPlace { delay: Duration::from_millis(50), ..Default::default() };
        let mut w = Outbox::new(&root, config(&root), &geo, status_with_workers(&root, 3)).unwrap();
        let stop = Mutex::new(w.waker());
        *geo.hook.lock().unwrap() = Some(Box::new(move || {
            let _ = stop.lock().unwrap().send(Wake::Stop);
        }));
        for i in 0..9 {
            make_photo(&root.join(format!("p{i}.jpg")), EIFFEL_TAGS);
        }
        w.scan_once();
        assert!(w.stopping);
        assert_eq!(count_files(&root.join(OUTBOX_DIR)), 3, "the three in hand finish");
        assert_eq!(w.pending.len(), 6);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn photos_in_hand_never_pick_the_same_name() {
        let dir = temp_dir("outbox-claims");
        std::fs::write(dir.join("a.jpg"), "").unwrap();
        let all = Mutex::new(HashSet::new());
        let mut one = Claims { all: &all, mine: Vec::new() };
        let mut two = Claims { all: &all, mine: Vec::new() };
        assert_eq!(one.free_name(&dir, "a.jpg".as_ref(), None), dir.join("a (2).jpg"));
        assert_eq!(two.free_name(&dir, "a.jpg".as_ref(), None), dir.join("a (3).jpg"), "(2) is claimed");
        // A RAW claims its stamped JPEG's name too, so a photo already called that moves aside.
        assert_eq!(one.free_name(&dir, "c.dng".as_ref(), Some(".jpg")), dir.join("c.dng"));
        assert_eq!(two.free_name(&dir, "c.dng.jpg".as_ref(), None), dir.join("c.dng (2).jpg"));
        drop(one);
        let mut three = Claims { all: &all, mine: Vec::new() };
        assert_eq!(three.free_name(&dir, "a.jpg".as_ref(), None), dir.join("a (2).jpg"), "given back");
        drop((two, three));
        assert!(all.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn waits_for_the_place_server_until_told_not_to() {
        let root = temp_dir("outbox-defer");
        let st = status(&root);
        let mut w = Outbox::new(&root, config(&root), &OfflineWithTown, st.clone()).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("b.jpg"), EIFFEL_TAGS);
        w.scan_once();
        assert!(root.join("a.jpg").exists() && root.join("b.jpg").exists(), "wait in place");
        assert_eq!(w.deferred[&root.join("a.jpg")].geo_attempts, 1);
        st.snapshot(|s| {
            let q = s.queue.iter().find(|q| q.path == "a.jpg").unwrap();
            assert!(matches!(&q.waiting, Waiting::Offline { near: Some(n), .. } if n == "Paris"), "{q:?}");
        });
        // Still unreachable: it keeps waiting (no giving up after a few tries).
        for _ in 0..5 {
            w.deferred.values_mut().for_each(|d| d.next_try = Instant::now());
            w.scan_once();
        }
        assert!(root.join("a.jpg").exists());

        // "Stamp now" for one photo: stamped with what this PC knows.
        w.stamp_now(Some(Path::new("a.jpg")));
        w.scan_once();
        assert!(stamped(&root.join("outbox/a.jpg")));
        assert!(root.join("b.jpg").exists(), "the other keeps waiting");
        st.snapshot(|s| assert_eq!(s.recent[0].caption[0], "Paris"));
        // ...and the waiting list remembers b.jpg across restarts.
        let saved: HashMap<String, u64> =
            serde_json::from_slice(&std::fs::read(root.join(".waiting.json")).unwrap()).unwrap();
        assert_eq!(saved.keys().collect::<Vec<_>>(), ["b.jpg"]);

        // Waited longer than the person allows: stamped with local data without being asked.
        let mut w = Outbox::new(&root, config(&root), &OfflineNoWait, st).unwrap();
        w.scan_once();
        assert!(stamped(&root.join("outbox/b.jpg")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The newest result the status has for `path`.
    fn result_for(status: &Status, path: &str) -> Entry {
        status.history(None, 100).into_iter().find(|e| e.path == path).unwrap()
    }

    #[test]
    fn stamps_again_with_a_written_caption_from_the_original() {
        let root = temp_dir("outbox-restamp");
        let status = status(&root);
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status.clone()).unwrap();
        std::fs::create_dir_all(root.join("trip")).unwrap();
        make_photo(&root.join("trip/a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("bare.jpg"), &[]);
        let bare = std::fs::read(root.join("bare.jpg")).unwrap();
        w.scan_once();
        let first = std::fs::read(root.join("outbox/trip/a.jpg")).unwrap();
        let original = std::fs::read(root.join("originals/trip/a.jpg")).unwrap();
        let mut renderer = Renderer::new();
        let originals = root.join(ORIGINALS_DIR);

        let entry = result_for(&status, "trip/a.jpg");
        let lines = vec!["Grandma's garden".to_owned(), "Summer 2024".to_owned()];
        let new = restamp(&status, &entry, lines.clone(), &originals, &mut renderer).unwrap();
        status.amend(new.clone(), entry.outcome);
        assert_eq!(new.caption, lines);
        let again = std::fs::read(root.join("outbox/trip/a.jpg")).unwrap();
        assert_ne!(again, first, "stamped copy replaced");
        assert!(stamped(&root.join("outbox/trip/a.jpg")));
        assert_eq!(std::fs::read(root.join("originals/trip/a.jpg")).unwrap(), original, "original untouched");
        assert_eq!(result_for(&status, "trip/a.jpg").caption, lines, "the history says so");

        // A photo moved unstamped can be stamped by hand: its original is put aside first.
        let entry = result_for(&status, "bare.jpg");
        assert_eq!(entry.outcome, Outcome::Unchanged);
        let new = restamp(&status, &entry, vec!["Somewhere".into()], &originals, &mut renderer).unwrap();
        status.amend(new.clone(), entry.outcome);
        assert_eq!(new.outcome, Outcome::Stamped);
        assert!(stamped(&root.join("outbox/bare.jpg")));
        assert_eq!(std::fs::read(root.join("originals/bare.jpg")).unwrap(), bare);
        status.snapshot(|s| assert_eq!((s.all_time.stamped, s.all_time.unchanged), (2, 0)));

        // A photo already stamped elsewhere has no original here.
        make_photo(&root.join("elsewhere.jpg"), EIFFEL_TAGS);
        stamp::stamp_in_place(
            &root.join("elsewhere.jpg"),
            &FixedPlace,
            &mut renderer,
            stamp::Options { dry_run: false, quality: 90 },
        )
        .unwrap();
        w.scan_once();
        let entry = result_for(&status, "elsewhere.jpg");
        let err = restamp(&status, &entry, vec!["x".into()], &originals, &mut renderer).unwrap_err();
        assert!(err.to_string().contains("already stamped"), "{err:#}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn photos_moved_out_of_the_outbox_leave_the_list() {
        let root = temp_dir("outbox-forget");
        let status = status(&root);
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status.clone()).unwrap();
        make_photo(&root.join("a.jpg"), EIFFEL_TAGS);
        make_photo(&root.join("b.jpg"), EIFFEL_TAGS);
        w.scan_once();
        assert_eq!(status.forget_gone(), 0, "both still in the outbox");
        std::fs::remove_file(root.join("outbox/a.jpg")).unwrap();
        assert_eq!(status.forget_gone(), 1);
        let left: Vec<String> = status.history(None, 100).into_iter().map(|e| e.path).collect();
        assert_eq!(left, ["b.jpg"]);
        status.clear_history();
        assert!(status.history(None, 100).is_empty());
        status.snapshot(|s| assert_eq!(s.all_time.stamped, 2, "all-time totals are kept"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn keeps_originals_beside_the_outbox() {
        let root = temp_dir("outbox-originals");
        let mut w = Outbox::new(&root, config(&root), &FixedPlace, status(&root)).unwrap();
        std::fs::create_dir_all(root.join("trip")).unwrap();
        make_photo(&root.join("trip/a.jpg"), EIFFEL_TAGS);
        let before = std::fs::read(root.join("trip/a.jpg")).unwrap();
        w.scan_once();
        assert!(stamped(&root.join("outbox/trip/a.jpg")));
        assert_eq!(
            std::fs::read(root.join("originals/trip/a.jpg")).unwrap(),
            before,
            "original kept as it was"
        );
        // The originals folder isn't scanned (nothing is stamped twice).
        w.scan_once();
        assert!(!stamped(&root.join("originals/trip/a.jpg")));
        assert!(w.pending.is_empty());
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
