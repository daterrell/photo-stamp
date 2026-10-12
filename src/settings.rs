//! Per-folder settings the dashboard can change, kept in `settings-<folder>.json` in photo-stamp's
//! data folder. Command-line options override them for that run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_QUALITY: u8 = 92;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where finished photos go; `None` is the "outbox" folder inside the watched folder.
    pub outbox: Option<PathBuf>,
    /// Where the originals of stamped photos are kept; `None` is the "originals" folder inside the
    /// watched folder.
    pub originals: Option<PathBuf>,
    /// Quality for lossy re-encoding (JPEG, HEIC, RAW→JPEG), 1–100.
    pub quality: u8,
    /// Seconds a file must stay unchanged before it's processed.
    pub settle_secs: u64,
    /// Seconds between full scans (backup for filesystem events).
    pub poll_secs: u64,
    /// Don't ask the place server: name places from this PC's towns database only.
    pub offline: bool,
    /// Show a notification when a batch of photos is done.
    pub notify: bool,
    /// Notice new photos but don't process them.
    pub paused: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            outbox: None,
            originals: None,
            quality: DEFAULT_QUALITY,
            settle_secs: 3,
            poll_secs: 30,
            offline: false,
            notify: true,
            paused: false,
        }
    }
}

impl Settings {
    pub fn path(data: &Path, key: &str) -> PathBuf {
        data.join(format!("settings-{key}.json"))
    }

    /// The saved settings, or the defaults if there are none (or they can't be read).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("ignoring unreadable settings {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Bring values into range.
    pub fn sanitized(mut self) -> Self {
        self.quality = self.quality.clamp(1, 100);
        self.settle_secs = self.settle_secs.min(3600);
        self.poll_secs = self.poll_secs.clamp(1, 3600);
        self.outbox = self.outbox.filter(|p| !p.as_os_str().is_empty());
        self
    }

    /// Whether going from `self` to `new` needs the service restarted to take effect.
    pub fn needs_restart(&self, new: &Settings) -> bool {
        self.outbox != new.outbox || self.originals != new.originals || self.offline != new.offline
    }
}

/// Command-line overrides: anything given wins over the saved settings.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub outbox: Option<PathBuf>,
    pub quality: Option<u8>,
    pub settle_secs: Option<u64>,
    pub poll_secs: Option<u64>,
    pub offline: bool,
}

impl Overrides {
    pub fn apply(&self, mut s: Settings) -> Settings {
        if let Some(o) = &self.outbox {
            s.outbox = Some(o.clone());
        }
        if let Some(q) = self.quality {
            s.quality = q;
        }
        if let Some(v) = self.settle_secs {
            s.settle_secs = v;
        }
        if let Some(v) = self.poll_secs {
            s.poll_secs = v;
        }
        s.offline |= self.offline;
        s.sanitized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::tests::temp_dir;

    #[test]
    fn saved_settings_round_trip_and_command_line_wins() {
        let dir = temp_dir("settings");
        let path = Settings::path(&dir, "k");
        assert_eq!(Settings::load(&path), Settings::default(), "nothing saved yet");
        let saved = Settings { quality: 80, notify: false, ..Settings::default() };
        saved.save(&path).unwrap();
        assert_eq!(Settings::load(&path), saved);

        let cli = Overrides { quality: Some(70), offline: true, ..Overrides::default() };
        let s = cli.apply(Settings::load(&path));
        assert_eq!((s.quality, s.offline, s.notify), (70, true, false));

        // Older or partial files fill in defaults.
        std::fs::write(&path, r#"{"quality": 0}"#).unwrap();
        let s = Overrides::default().apply(Settings::load(&path));
        assert_eq!((s.quality, s.poll_secs), (1, 30));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_outbox_and_offline_need_a_restart() {
        let a = Settings::default();
        assert!(!a.needs_restart(&Settings { quality: 50, paused: true, ..a.clone() }));
        assert!(a.needs_restart(&Settings { offline: true, ..a.clone() }));
        assert!(a.needs_restart(&Settings { outbox: Some("D:/x".into()), ..a.clone() }));
    }
}
