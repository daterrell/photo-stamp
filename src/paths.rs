//! Where photo-stamp keeps its own files (place cache, state, log).

use std::path::{Path, PathBuf};

/// A short stable key for a watched folder (case-insensitive, as Windows paths are), used to name
/// its single-instance lock, stop signal, startup entry and state file.
pub fn folder_key(root: &Path) -> String {
    // FNV-1a: tiny, stable across builds (unlike std's hasher).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
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
