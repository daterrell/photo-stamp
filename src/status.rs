//! What the drop-folder service is doing, for the dashboard: the photo being processed, the queue,
//! recent results, totals, an ETA, recent log lines and health. The watcher reports into a
//! [`Status`]; the local API ([`crate::api`]) reads snapshots of it and streams its events.
//!
//! Results are kept in `history-<folder>.jsonl` (the most recent few thousand) and all-time totals
//! in `totals-<folder>.json`, so the dashboard has a history after a restart. Small before/after
//! previews of stamped photos are kept in `thumbs/<folder>/`.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use image::DynamicImage;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::geo::GeoHealth;
use crate::settings::Settings;

const RECENT_RESULTS: usize = 500;
const HISTORY_MAX: usize = 5000;
const HISTORY_KEEP: usize = 4000;
const LOG_LINES: usize = 1000;
const THUMBS_KEPT: u64 = 2000;
const THUMB_PX: u32 = 640;
/// Per-photo time assumed before any photo has been timed.
const DEFAULT_SECS: f64 = 5.0;
/// Weight of the newest sample in the moving averages.
const EMA_ALPHA: f64 = 0.3;
/// Events buffered per dashboard connection before it's considered stuck and dropped.
const SUBSCRIBER_BUFFER: usize = 1024;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// What happened to a photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Stamped,
    /// Moved to the outbox as it was (already stamped, or nothing to stamp).
    Unchanged,
    /// Couldn't be processed; left where it was.
    Failed,
}

/// One finished photo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    /// Path within the watched folder.
    pub path: String,
    pub outcome: Outcome,
    /// Why it wasn't stamped (unchanged/failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caption: Vec<String>,
    /// Where it ended up, relative to the outbox's parent (e.g. `outbox\trip\a.jpg`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub ms: u64,
    /// Map data downloaded for this photo.
    #[serde(default)]
    pub downloaded: u64,
    /// Whether before/after previews were saved.
    #[serde(default)]
    pub thumbs: bool,
    pub finished: u64,
    /// Where the original was kept (relative to the outbox's parent), so it can be stamped again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
    /// Where it was taken: latitude, longitude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps: Option<[f64; 2]>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub stamped: u64,
    pub unchanged: u64,
    pub failed: u64,
    pub downloaded: u64,
    /// Time spent processing, for an average.
    pub ms: u64,
}

impl Totals {
    fn add(&mut self, e: &Entry) {
        match e.outcome {
            Outcome::Stamped => self.stamped += 1,
            Outcome::Unchanged => self.unchanged += 1,
            Outcome::Failed => self.failed += 1,
        }
        self.downloaded += e.downloaded;
        self.ms += e.ms;
    }

    pub fn count(&self) -> u64 {
        self.stamped + self.unchanged + self.failed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Reading metadata and working out the place.
    Locating,
    /// Decoding and drawing the caption.
    Drawing,
    /// Encoding and writing to the outbox.
    Saving,
    Moving,
}

#[derive(Debug, Clone, Serialize)]
pub struct Current {
    pub id: u64,
    pub path: String,
    pub stage: Stage,
    pub started: u64,
    /// Whether the "before" preview is ready.
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Waiting {
    /// Still being copied (or just arrived).
    Settling,
    Ready,
    /// Processing is paused.
    Paused,
    /// The place server couldn't be reached; asked again at `until` (ms since the epoch).
    Offline {
        until: u64,
        /// The nearest town, from the local towns database.
        #[serde(skip_serializing_if = "Option::is_none")]
        near: Option<String>,
        /// When it'll be stamped with local place data if the server still can't be reached.
        #[serde(skip_serializing_if = "Option::is_none")]
        stamp_by: Option<u64>,
    },
    /// Open in another program; retried at `until`.
    Busy {
        until: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Queued {
    pub path: String,
    #[serde(flatten)]
    pub waiting: Waiting,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub at: u64,
    pub level: &'static str,
    pub text: String,
}

/// The photos since the queue was last empty: the progress bar's denominator.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Batch {
    pub done: u64,
    pub stamped: u64,
    pub unchanged: u64,
    pub failed: u64,
    pub started: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    /// Estimated seconds until the queue is empty (None when idle).
    pub eta_secs: Option<u64>,
    /// Photos per minute over this batch.
    pub per_min: Option<f64>,
}

struct Inner {
    seq: u64,
    next_id: u64,
    current: Option<Current>,
    queue: Vec<Queued>,
    recent: VecDeque<Entry>,
    session: Totals,
    all_time: Totals,
    batch: Batch,
    /// Moving averages of seconds per photo at new spots (map data downloaded) and repeat spots.
    ema_new: Option<f64>,
    ema_repeat: Option<f64>,
    new_spots: u64,
    log: VecDeque<LogLine>,
    health: GeoHealth,
    settings: Settings,
    /// Lines in the history file, to know when to trim it.
    history_lines: usize,
}

/// Called with a summary when a batch finishes (title, body).
pub type Notifier = Box<dyn Fn(&str, &str) + Send + Sync>;

pub struct Status {
    inner: Mutex<Inner>,
    subscribers: Mutex<Vec<SyncSender<Arc<str>>>>,
    paused: AtomicBool,
    root: PathBuf,
    outbox: PathBuf,
    /// Where history, totals and thumbnails live; `None` keeps everything in memory.
    store: Option<Store>,
    started: u64,
    notifier: Mutex<Option<Notifier>>,
}

struct Store {
    history: PathBuf,
    totals: PathBuf,
    thumbs: PathBuf,
}

/// The `/api/status` snapshot.
#[derive(Serialize)]
pub struct Snapshot<'a> {
    pub seq: u64,
    pub version: &'static str,
    pub root: &'a Path,
    pub outbox: &'a Path,
    pub paused: bool,
    pub started: u64,
    pub current: Option<Current>,
    pub queue: Vec<Queued>,
    pub progress: Progress,
    pub batch: Batch,
    pub session: Totals,
    pub all_time: Totals,
    pub recent: Vec<Entry>,
    pub log: Vec<LogLine>,
    pub health: GeoHealth,
    pub settings: Settings,
}

impl Status {
    /// `data`: photo-stamp's data folder (history and thumbnails go there); `key` names this
    /// watched folder's files.
    pub fn new(root: &Path, outbox: &Path, data: Option<&Path>, key: &str, settings: Settings) -> Self {
        let store = data.map(|d| Store {
            history: d.join(format!("history-{key}.jsonl")),
            totals: d.join(format!("totals-{key}.json")),
            thumbs: d.join("thumbs").join(key),
        });
        let mut recent = VecDeque::new();
        let mut history_lines = 0;
        let mut all_time = Totals::default();
        if let Some(s) = &store {
            if let Ok(text) = std::fs::read_to_string(&s.history) {
                for line in text.lines() {
                    history_lines += 1;
                    if let Ok(e) = serde_json::from_str::<Entry>(line) {
                        if recent.len() == RECENT_RESULTS {
                            recent.pop_front();
                        }
                        recent.push_back(e);
                    }
                }
            }
            all_time = std::fs::read_to_string(&s.totals)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
        }
        let next_id = recent.back().map_or(1, |e: &Entry| e.id + 1);
        let paused = settings.paused;
        Self {
            inner: Mutex::new(Inner {
                seq: 0,
                next_id,
                current: None,
                queue: Vec::new(),
                recent,
                session: Totals::default(),
                all_time,
                batch: Batch::default(),
                ema_new: None,
                ema_repeat: None,
                new_spots: 0,
                log: VecDeque::new(),
                health: GeoHealth::default(),
                settings,
                history_lines,
            }),
            subscribers: Mutex::new(Vec::new()),
            paused: AtomicBool::new(paused),
            root: root.to_owned(),
            outbox: outbox.to_owned(),
            store,
            started: now_ms(),
            notifier: Mutex::new(None),
        }
    }

    pub fn set_notifier(&self, n: Notifier) {
        *self.notifier.lock().unwrap() = Some(n);
    }

    /// A stream of JSON events (`{"type": …, "seq": …}`) for one dashboard connection.
    pub fn subscribe(&self) -> Receiver<Arc<str>> {
        let (tx, rx) = sync_channel(SUBSCRIBER_BUFFER);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    fn emit(&self, inner: &mut Inner, kind: &str, mut data: serde_json::Value) {
        inner.seq += 1;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("type".into(), kind.into());
            obj.insert("seq".into(), inner.seq.into());
        }
        let msg: Arc<str> = data.to_string().into();
        // Drop connections that have gone away or stopped reading.
        self.subscribers.lock().unwrap().retain(|tx| match tx.try_send(msg.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.settings.paused = paused;
        self.emit(&mut inner, "paused", json!({ "paused": paused }));
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        self.paused.store(settings.paused, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.settings = settings.clone();
        self.emit(&mut inner, "settings", json!({ "settings": settings }));
    }

    pub fn set_health(&self, health: GeoHealth) {
        let mut inner = self.inner.lock().unwrap();
        if inner.health != health {
            inner.health = health.clone();
            self.emit(&mut inner, "health", json!({ "health": health }));
        }
    }

    /// The photos waiting, as of the latest scan.
    pub fn set_queue(&self, queue: Vec<Queued>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue == queue {
            return;
        }
        if inner.queue.is_empty() && inner.current.is_none() && !queue.is_empty() {
            inner.batch = Batch { started: now_ms(), ..Batch::default() };
        }
        inner.queue = queue;
        let (queue, progress) = (inner.queue.clone(), progress(&inner));
        self.emit(&mut inner, "queue", json!({ "queue": queue, "progress": progress }));
    }

    /// A photo is being processed; returns its id.
    pub fn start(&self, path: &str) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        if inner.queue.is_empty() && inner.batch.done == 0 {
            inner.batch.started = now_ms();
        }
        inner.queue.retain(|q| q.path != path);
        let current =
            Current { id, path: path.to_owned(), stage: Stage::Locating, started: now_ms(), preview: false };
        inner.current = Some(current.clone());
        let progress = progress(&inner);
        self.emit(&mut inner, "started", json!({ "current": current, "progress": progress }));
        id
    }

    pub fn stage(&self, stage: Stage) {
        let mut inner = self.inner.lock().unwrap();
        let Some(current) = inner.current.as_mut() else { return };
        current.stage = stage;
        let (id, current) = (current.id, current.clone());
        self.emit(&mut inner, "stage", json!({ "id": id, "current": current }));
    }

    /// The current photo was put back in the queue (to retry later).
    pub fn deferred(&self, queued: Queued) {
        let mut inner = self.inner.lock().unwrap();
        // Its id is used up; it gets a new one when it's tried again.
        let id = inner.current.take().map(|c| c.id);
        inner.queue.retain(|q| q.path != queued.path);
        inner.queue.push(queued.clone());
        let progress = progress(&inner);
        self.emit(&mut inner, "deferred", json!({ "id": id, "queued": queued, "progress": progress }));
    }

    pub fn thumb_path(&self, id: u64, which: Which) -> Option<PathBuf> {
        let s = self.store.as_ref()?;
        Some(s.thumbs.join(format!("{id}-{}.jpg", which.name())))
    }

    /// Save a small preview of the photo being processed.
    pub fn save_thumb(&self, id: u64, which: Which, img: &DynamicImage) {
        let Some(path) = self.thumb_path(id, which) else { return };
        let write = || -> anyhow::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
            let small = img.thumbnail(THUMB_PX, THUMB_PX).into_rgb8();
            let tmp = path.with_extension("tmp");
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75).encode_image(&small)?;
            out.flush()?;
            drop(out);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write() {
            log::warn!("couldn't save a preview: {e:#}");
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if which == Which::Before
            && let Some(c) = inner.current.as_mut().filter(|c| c.id == id)
        {
            c.preview = true;
        }
        self.emit(&mut inner, "preview", json!({ "id": id, "which": which.name() }));
    }

    /// The current photo is finished.
    pub fn finish(&self, mut entry: Entry) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(c) = inner.current.take() {
            entry.id = c.id;
            entry.ms = now_ms().saturating_sub(c.started);
        }
        entry.finished = now_ms();
        inner.session.add(&entry);
        inner.all_time.add(&entry);
        inner.batch.done += 1;
        match entry.outcome {
            Outcome::Stamped => inner.batch.stamped += 1,
            Outcome::Unchanged => inner.batch.unchanged += 1,
            Outcome::Failed => inner.batch.failed += 1,
        }
        // Only stamped photos say much about how long the next will take.
        if entry.outcome == Outcome::Stamped {
            let secs = entry.ms as f64 / 1000.0;
            let ema = if entry.downloaded > 0 {
                inner.new_spots += 1;
                &mut inner.ema_new
            } else {
                &mut inner.ema_repeat
            };
            *ema = Some(ema.map_or(secs, |e| e + EMA_ALPHA * (secs - e)));
        }
        if inner.recent.len() == RECENT_RESULTS {
            inner.recent.pop_front();
        }
        inner.recent.push_back(entry.clone());
        self.persist(&mut inner, &entry);
        let progress = progress(&inner);
        let (session, batch) = (inner.session, inner.batch);
        self.emit(
            &mut inner,
            "done",
            json!({ "entry": entry, "progress": progress, "session": session, "batch": batch }),
        );

        // The batch is over: tell the user.
        if inner.queue.is_empty() {
            let batch = std::mem::take(&mut inner.batch);
            let notify = inner.settings.notify;
            self.emit(&mut inner, "idle", json!({ "batch": batch }));
            drop(inner);
            if notify && let Some(n) = self.notifier.lock().unwrap().as_ref() {
                let (title, body) = batch_summary(&batch);
                n(&title, &body);
            }
        }
    }

    fn persist(&self, inner: &mut Inner, entry: &Entry) {
        let Some(s) = &self.store else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = s.history.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&s.history)?;
            writeln!(f, "{}", serde_json::to_string(entry)?)?;
            std::fs::write(&s.totals, serde_json::to_vec(&inner.all_time)?)
        };
        if let Err(e) = write() {
            log::warn!("couldn't save history: {e}");
        }
        inner.history_lines += 1;
        if inner.history_lines > HISTORY_MAX {
            inner.history_lines = trim_history(&s.history, HISTORY_KEEP).unwrap_or(inner.history_lines);
        }
        // Previews are kept for the most recent photos only.
        if let Some(old) = entry.id.checked_sub(THUMBS_KEPT) {
            for which in [Which::Before, Which::After] {
                let _ = std::fs::remove_file(s.thumbs.join(format!("{old}-{}.jpg", which.name())));
            }
        }
    }

    /// A line from the log.
    pub fn log(&self, level: &'static str, text: &str) {
        let line = LogLine { at: now_ms(), level, text: text.to_owned() };
        let Ok(mut inner) = self.inner.try_lock() else {
            // Logged while the status itself is busy (e.g. a warning from inside it): skip the
            // dashboard copy rather than deadlock; it's still in the log file.
            return;
        };
        if inner.log.len() == LOG_LINES {
            inner.log.pop_front();
        }
        inner.log.push_back(line.clone());
        self.emit(&mut inner, "log", json!({ "line": line }));
    }

    /// Results older than `before` (an id), newest first.
    pub fn history(&self, before: Option<u64>, limit: usize) -> Vec<Entry> {
        let in_memory: Vec<Entry> = {
            let inner = self.inner.lock().unwrap();
            inner
                .recent
                .iter()
                .rev()
                .filter(|e| before.is_none_or(|b| e.id < b))
                .take(limit)
                .cloned()
                .collect()
        };
        if in_memory.len() == limit || self.store.is_none() {
            return in_memory;
        }
        // Older than what's kept in memory: read the file.
        let Some(s) = &self.store else { return in_memory };
        let Ok(text) = std::fs::read_to_string(&s.history) else { return in_memory };
        let mut all: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .filter(|e| before.is_none_or(|b| e.id < b))
            .collect();
        all.reverse();
        all.truncate(limit);
        all
    }

    /// Everything the dashboard shows, at once.
    pub fn snapshot<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        let inner = self.inner.lock().unwrap();
        let snap = Snapshot {
            seq: inner.seq,
            version: photo_stamp_http::VERSION,
            root: &self.root,
            outbox: &self.outbox,
            paused: self.is_paused(),
            started: self.started,
            current: inner.current.clone(),
            queue: inner.queue.clone(),
            progress: progress(&inner),
            batch: inner.batch,
            session: inner.session,
            all_time: inner.all_time,
            recent: inner.recent.iter().rev().take(100).cloned().collect(),
            log: inner.log.iter().rev().take(200).rev().cloned().collect(),
            health: inner.health.clone(),
            settings: inner.settings.clone(),
        };
        f(&snap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Before,
    After,
}

impl Which {
    pub fn name(self) -> &'static str {
        match self {
            Which::Before => "before",
            Which::After => "after",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "before" => Some(Which::Before),
            "after" => Some(Which::After),
            _ => None,
        }
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "21 photos stamped", "2 moved unchanged, 1 couldn't be read".
pub fn batch_summary(b: &Batch) -> (String, String) {
    let title = match b.stamped {
        0 => "No photos stamped".to_owned(),
        n => format!("{} stamped", plural(n, "photo", "photos")),
    };
    let mut parts = Vec::new();
    if b.unchanged > 0 {
        parts.push(format!("{} moved unchanged", b.unchanged));
    }
    if b.failed > 0 {
        parts.push(format!("{} couldn't be processed", b.failed));
    }
    let body =
        if parts.is_empty() { "All in the outbox.".to_owned() } else { format!("{}.", parts.join(", ")) };
    (title, body)
}

/// Batch progress and an estimate of the time left.
fn progress(inner: &Inner) -> Progress {
    let waiting = inner.queue.len() as u64 + u64::from(inner.current.is_some());
    let done = inner.batch.done;
    if waiting == 0 {
        return Progress { done, total: done, eta_secs: None, per_min: None };
    }
    // Expected time per photo: mix the new-spot and repeat-spot averages by how often photos
    // have needed map data so far.
    let stamped = inner.session.stamped.max(1) as f64;
    let share_new = (inner.new_spots as f64 / stamped).clamp(0.0, 1.0);
    let per_photo = match (inner.ema_new, inner.ema_repeat) {
        (Some(n), Some(r)) => share_new * n + (1.0 - share_new) * r,
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => DEFAULT_SECS,
    };
    let mut eta = waiting as f64 * per_photo;
    // The photo in hand is partly done.
    if let Some(c) = &inner.current {
        let elapsed = now_ms().saturating_sub(c.started) as f64 / 1000.0;
        eta -= elapsed.min(per_photo * 0.9);
    }
    let per_min = (done > 0 && inner.batch.started > 0).then(|| {
        let mins = now_ms().saturating_sub(inner.batch.started) as f64 / 60_000.0;
        done as f64 / mins.max(1.0 / 60.0)
    });
    Progress { done, total: done + waiting, eta_secs: Some(eta.max(1.0).round() as u64), per_min }
}

/// Keep the last `keep` lines of the history file; returns how many remain.
fn trim_history(path: &Path, keep: usize) -> std::io::Result<usize> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let kept = &lines[lines.len().saturating_sub(keep)..];
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, kept.join("\n") + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(kept.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::tests::temp_dir;

    fn status(data: Option<&Path>) -> Status {
        Status::new(Path::new("/drop"), Path::new("/drop/outbox"), data, "k", Settings::default())
    }

    fn entry(outcome: Outcome, downloaded: u64) -> Entry {
        Entry {
            id: 0,
            path: "a.jpg".into(),
            outcome,
            reason: None,
            caption: vec!["Somewhere".into()],
            output: Some("outbox/a.jpg".into()),
            ms: 0,
            downloaded,
            thumbs: false,
            finished: 0,
            original: None,
            gps: None,
        }
    }

    fn queued(path: &str) -> Queued {
        Queued { path: path.into(), waiting: Waiting::Ready }
    }

    #[test]
    fn progress_counts_the_batch_and_estimates_time_left() {
        let s = status(None);
        let events = s.subscribe();
        s.set_queue(vec![queued("a.jpg"), queued("b.jpg"), queued("c.jpg")]);
        s.snapshot(|snap| {
            assert_eq!((snap.progress.done, snap.progress.total), (0, 3));
            assert_eq!(snap.progress.eta_secs, Some(15), "5 s each until something has been timed");
        });
        s.start("a.jpg");
        s.snapshot(|snap| {
            assert_eq!(snap.queue.len(), 2, "the photo in hand leaves the queue");
            assert_eq!(snap.progress.total, 3);
        });
        let mut e = entry(Outcome::Stamped, 0);
        e.ms = 2000;
        s.finish(e);
        // `finish` times the photo itself; pretend the repeat-spot average is 2 s.
        s.inner.lock().unwrap().ema_repeat = Some(2.0);
        s.snapshot(|snap| {
            assert_eq!((snap.progress.done, snap.progress.total), (1, 3));
            assert_eq!(snap.progress.eta_secs, Some(4));
            assert_eq!(snap.session.stamped, 1);
        });
        let kinds: Vec<String> = events
            .try_iter()
            .map(|m| {
                serde_json::from_str::<serde_json::Value>(&m).unwrap()["type"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(kinds, ["queue", "started", "done"]);
    }

    #[test]
    fn a_drained_queue_ends_the_batch_and_notifies() {
        let s = status(None);
        let said = Arc::new(Mutex::new(Vec::new()));
        let sink = said.clone();
        s.set_notifier(Box::new(move |t, b| sink.lock().unwrap().push(format!("{t} / {b}"))));
        s.set_queue(vec![queued("a.jpg"), queued("b.jpg")]);
        s.start("a.jpg");
        s.finish(entry(Outcome::Stamped, 10));
        assert!(said.lock().unwrap().is_empty(), "b.jpg is still waiting");
        s.start("b.jpg");
        s.finish(entry(Outcome::Unchanged, 0));
        assert_eq!(*said.lock().unwrap(), ["1 photo stamped / 1 moved unchanged."]);
        s.snapshot(|snap| assert_eq!(snap.batch.done, 0, "next batch starts from zero"));
    }

    #[test]
    fn history_survives_a_restart_and_is_trimmed() {
        let dir = temp_dir("status-history");
        {
            let s = status(Some(&dir));
            for _ in 0..3 {
                s.start("a.jpg");
                s.finish(entry(Outcome::Stamped, 1));
            }
        }
        let s = status(Some(&dir));
        s.snapshot(|snap| {
            assert_eq!(snap.recent.len(), 3);
            assert_eq!(snap.recent[0].id, 3, "newest first");
            assert_eq!(snap.all_time.stamped, 3);
            assert_eq!(snap.session.stamped, 0);
        });
        assert_eq!(s.start("b.jpg"), 4, "ids carry on");
        assert_eq!(s.history(Some(3), 10).iter().map(|e| e.id).collect::<Vec<_>>(), [2, 1]);

        let path = dir.join("history-k.jsonl");
        assert_eq!(trim_history(&path, 2).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn previews_are_saved_and_announced() {
        let dir = temp_dir("status-thumbs");
        let s = status(Some(&dir));
        let id = s.start("a.jpg");
        s.save_thumb(id, Which::Before, &DynamicImage::new_rgb8(2000, 1000));
        let saved = image::open(s.thumb_path(id, Which::Before).unwrap()).unwrap();
        assert_eq!((saved.width(), saved.height()), (640, 320));
        s.snapshot(|snap| assert!(snap.current.as_ref().unwrap().preview));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn summaries_read_naturally() {
        let b = Batch { done: 3, stamped: 3, ..Batch::default() };
        assert_eq!(batch_summary(&b), ("3 photos stamped".into(), "All in the outbox.".into()));
        let b = Batch { done: 2, unchanged: 1, failed: 1, ..Batch::default() };
        assert_eq!(batch_summary(&b).1, "1 moved unchanged, 1 couldn't be processed.");
    }
}
