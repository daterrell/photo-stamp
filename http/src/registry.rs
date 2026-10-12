//! How the dashboard finds running photo-stamp services: each one writes
//! `<data dir>/services/<folder key>.json` with its port and access token. The data dir is per
//! user (`%LOCALAPPDATA%\photo-stamp`), so only that user can read the token.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    /// Stable key of the watched folder.
    pub key: String,
    pub root: PathBuf,
    pub outbox: PathBuf,
    /// The program, so the dashboard can start it again.
    pub exe: Option<PathBuf>,
    pub pid: u32,
    /// Where its API listens on 127.0.0.1 (`None` once stopped).
    pub port: Option<u16>,
    /// Required as `Authorization: Bearer <token>`.
    pub token: String,
    pub version: String,
    /// Milliseconds since the epoch.
    pub started: u64,
    pub running: bool,
}

/// `%LOCALAPPDATA%\photo-stamp` on Windows; `$XDG_CACHE_HOME/photo-stamp` or `~/.cache/photo-stamp`
/// elsewhere. `PHOTO_STAMP_DATA_DIR` overrides it (tests use this to stay away from a real install).
/// `None` only if no suitable environment variable is set.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PHOTO_STAMP_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.map(|b| b.join("photo-stamp"))
}

/// A short stable key for a watched folder (case-insensitive, as Windows paths are), used to name
/// its single-instance lock, stop signal, startup entry, registration and settings.
pub fn folder_key(root: &Path) -> String {
    // FNV-1a: tiny, stable across builds (unlike std's hasher).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn services_dir(data: &Path) -> PathBuf {
    data.join("services")
}

impl Registration {
    pub fn path(data: &Path, key: &str) -> PathBuf {
        services_dir(data).join(format!("{key}.json"))
    }

    pub fn save(&self, data: &Path) -> std::io::Result<()> {
        let path = Self::path(data, &self.key);
        std::fs::create_dir_all(services_dir(data))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn load(data: &Path, key: &str) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path(data, key)).ok()?).ok()
    }

    /// Every service that has registered, running or not, ordered by folder.
    pub fn all(data: &Path) -> Vec<Self> {
        let mut all: Vec<Self> = std::fs::read_dir(services_dir(data))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
                    .collect()
            })
            .unwrap_or_default();
        all.sort_by(|a: &Self, b| a.root.cmp(&b.root));
        all
    }
}

/// Compare secrets without leaking how much matched through timing.
pub fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_round_trip_and_list() {
        let dir = std::env::temp_dir().join(format!("ps-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reg = |root: &str| Registration {
            key: folder_key(Path::new(root)),
            root: root.into(),
            outbox: Path::new(root).join("outbox"),
            exe: None,
            pid: 1,
            port: Some(1234),
            token: "t".into(),
            version: "0".into(),
            started: 0,
            running: true,
        };
        reg("/b").save(&dir).unwrap();
        reg("/a").save(&dir).unwrap();
        std::fs::write(services_dir(&dir).join("junk.json"), "not json").unwrap();
        let all = Registration::all(&dir);
        assert_eq!(all.iter().map(|r| r.root.to_str().unwrap()).collect::<Vec<_>>(), ["/a", "/b"]);
        assert_eq!(Registration::load(&dir, &folder_key(Path::new("/a"))), Some(reg("/a")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_keys_ignore_case_and_trailing_slashes() {
        assert_eq!(folder_key(Path::new(r"C:\Photos\")), folder_key(Path::new(r"c:\photos")));
        assert_ne!(folder_key(Path::new("/a")), folder_key(Path::new("/b")));
    }

    #[test]
    fn secrets_compare() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
    }
}
