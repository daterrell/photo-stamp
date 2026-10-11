//! Where photo-stamp keeps its own files (place cache, state, log).

use std::path::PathBuf;

/// `%LOCALAPPDATA%\photo-stamp` on Windows; `$XDG_CACHE_HOME/photo-stamp` or `~/.cache/photo-stamp`
/// elsewhere. `None` only if no suitable environment variable is set.
pub fn data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.map(|b| b.join("photo-stamp"))
}
