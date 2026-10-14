//! Bakes the version into every photo-stamp program as `PHOTO_STAMP_VERSION` (see
//! `photo_stamp_http::VERSION`). The version comes from git tags via `scripts/version.sh`, or
//! from `PHOTO_STAMP_VERSION` when set (CI and Docker builds, which can't see .git). The
//! `version` in Cargo.toml isn't used.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PHOTO_STAMP_VERSION");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let script = repo.join("scripts/version.sh");
    // Rebuild when HEAD moves or tags change.
    for args in [["rev-parse", "--git-dir"], ["rev-parse", "--git-common-dir"]] {
        if let Some(dir) =
            Command::new("git").current_dir(&repo).args(args).output().ok().filter(|o| o.status.success())
        {
            let dir = repo.join(String::from_utf8_lossy(&dir.stdout).trim());
            for f in ["HEAD", "refs/tags", "packed-refs", "index"] {
                if dir.join(f).exists() {
                    println!("cargo:rerun-if-changed={}", dir.join(f).display());
                }
            }
        }
    }
    let version = match std::env::var("PHOTO_STAMP_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_owned(),
        _ => Command::new("bash")
            .arg(&script)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "0.0.0-dev".to_owned()),
    };
    println!("cargo:rustc-env=PHOTO_STAMP_VERSION={version}");
}
