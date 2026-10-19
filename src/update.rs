//! Updating photo-stamp from its releases on GitHub (Windows installs).
//!
//! 1. **Check** (every 6 hours, or when asked): GitHub says what the newest release is (tagged,
//!    not a draft or prerelease), with the SHA-256 of its Windows zip.
//! 2. **Download** a newer one in the background, check its SHA-256 and unpack it into
//!    `<data>/updates/<version>/`. The dashboard then offers to install it.
//! 3. **Install** (only when asked, from the dashboard): the watcher finishes the photo in hand and
//!    stops; each program file is renamed to `<name>.old` (Windows allows that even for a running
//!    program) and the new one copied in; the new version is started. This process stays on as a
//!    watchdog: if the new version hasn't come up within [`HANDOFF_WAIT`], it's stopped, the
//!    `.old` files are put back and the old version is started again.
//! 4. The new version, once it's been running a while, removes the `.old` files and the download.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use photo_stamp_proto::version::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::remote::RemoteGeocoder;
use crate::status::{Status, now_ms};

/// How long the new version has to start and register before going back.
pub const HANDOFF_WAIT: Duration = Duration::from_secs(90);
/// The new version cleans up after itself once it's been running this long.
const SETTLED_AFTER: Duration = Duration::from_secs(120);
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// The folder inside the Windows zip.
const PACKAGE_DIR: &str = "photo-stamp-windows";
/// The newest release, from GitHub's API. `PHOTO_STAMP_RELEASES_URL` points elsewhere (tests).
const RELEASES_URL: &str = "https://api.github.com/repos/daterrell/photo-stamp/releases/latest";

/// What GitHub says about a release (the parts used).
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// "sha256:<hex>".
    #[serde(default)]
    digest: Option<String>,
}

/// A release's Windows build: its version, where to download the zip, and the zip's SHA-256.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

impl Release {
    fn offer(self) -> anyhow::Result<Offer> {
        let version = self.tag_name.trim_start_matches('v').to_owned();
        let zip = self
            .assets
            .into_iter()
            .find(|a| a.name.starts_with("photo-stamp-windows-") && a.name.ends_with(".zip"))
            .with_context(|| format!("release {} has no Windows zip", self.tag_name))?;
        let sha256 = zip
            .digest
            .as_deref()
            .and_then(|d| d.strip_prefix("sha256:"))
            .with_context(|| format!("release {} doesn't say the checksum of its zip", self.tag_name))?
            .to_owned();
        Ok(Offer { version, url: zip.browser_download_url, sha256 })
    }
}

fn agent(recv_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(recv_timeout))
        .user_agent(format!("photo-stamp/{}", photo_stamp_http::VERSION))
        .build()
        .into()
}

/// The newest release's Windows build (`None`: nothing released yet).
pub fn latest_release() -> anyhow::Result<Option<Offer>> {
    let url = std::env::var("PHOTO_STAMP_RELEASES_URL").unwrap_or_else(|_| RELEASES_URL.to_owned());
    let mut resp = agent(Duration::from_secs(30))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("can't reach GitHub to check for updates")?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        403 | 429 => bail!("GitHub is limiting update checks right now; it'll be tried again later"),
        status => bail!("GitHub answered HTTP {status} when checking for updates"),
    }
    let release: Release = serde_json::from_slice(&resp.body_mut().read_to_vec()?)
        .context("GitHub's answer about the latest release didn't make sense")?;
    release.offer().map(Some)
}

/// Download `url` into `dest`, reporting progress; returns its SHA-256 (hex).
fn download(url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    // Big and slow is fine here; stalled isn't.
    let mut resp = agent(Duration::from_secs(60)).get(url).call().context("can't download the update")?;
    anyhow::ensure!(resp.status() == 200, "the download answered HTTP {}", resp.status());
    let total =
        resp.headers().get("content-length").and_then(|h| h.to_str().ok()).and_then(|s| s.parse().ok());
    let mut body = resp.body_mut().with_config().limit(1 << 30).reader();
    let mut out = std::fs::File::create(dest)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    out.flush()?;
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// What the updater is doing, for the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// Updates don't apply here (e.g. not a Windows install, or no place server).
    Off {
        reason: String,
    },
    Idle,
    Checking,
    UpToDate {
        checked: u64,
    },
    Downloading {
        version: String,
        done: u64,
        total: Option<u64>,
    },
    /// Downloaded and checked; waiting to be installed.
    Ready {
        version: String,
    },
    Installing {
        version: String,
    },
    Failed {
        error: String,
        checked: u64,
    },
}

/// What was installed, kept in `<data>/updates/install.json` until the new version has settled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Install {
    pub from: String,
    pub to: String,
    /// Paths (relative to the program's folder) that were replaced or added.
    pub files: Vec<String>,
    /// Set by the watchdog when it went back to the old version.
    #[serde(default)]
    pub rolled_back: bool,
}

pub struct Updater {
    geo: Arc<RemoteGeocoder>,
    status: Arc<Status>,
    /// `<data>/updates`.
    dir: PathBuf,
    /// The program's folder (where photo-stamp.exe is).
    program_dir: PathBuf,
    state: Mutex<State>,
    /// One check or download at a time.
    busy: Mutex<()>,
    install_requested: AtomicBool,
    /// Why updates can't be installed here, if they can't.
    unsupported: Option<String>,
}

fn copy_tree_files(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();
    for e in walkdir::WalkDir::new(root).min_depth(1) {
        let e = e?;
        if e.file_type().is_file() {
            let rel = e.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            files.push(rel);
        }
    }
    files.sort();
    Ok(files)
}

fn old_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Put the files of `staged` (an unpacked build) into `program_dir`: each existing file is first
/// renamed to `<name>.old`. Returns the files placed. On failure, what was done is undone.
pub fn swap_in(staged: &Path, program_dir: &Path) -> anyhow::Result<Vec<String>> {
    let files = copy_tree_files(staged)?;
    let mut done: Vec<String> = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        for rel in &files {
            let dest = program_dir.join(rel);
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if dest.exists() {
                let old = old_of(&dest);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&dest, &old).with_context(|| format!("setting aside {rel}"))?;
            }
            done.push(rel.clone());
            std::fs::copy(staged.join(rel), &dest).with_context(|| format!("putting in {rel}"))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        roll_back(program_dir, &done);
        return Err(e);
    }
    Ok(files)
}

/// Undo [`swap_in`]: the new files go, the `.old` ones come back.
pub fn roll_back(program_dir: &Path, files: &[String]) {
    for rel in files {
        let dest = program_dir.join(rel);
        let old = old_of(&dest);
        if old.exists() {
            let _ = std::fs::remove_file(&dest);
            if let Err(e) = std::fs::rename(&old, &dest) {
                log::warn!("couldn't put back {rel}: {e}");
            }
        } else {
            // Added by the update.
            let _ = std::fs::remove_file(&dest);
        }
    }
}

/// The update is staying: remove the `.old` files.
pub fn settle(program_dir: &Path, files: &[String]) {
    for rel in files {
        let _ = std::fs::remove_file(old_of(&program_dir.join(rel)));
    }
}

/// Unpack a downloaded build; returns the folder holding its files (`<dest>/photo-stamp-windows`).
pub fn unpack(zip_path: &Path, dest: &Path) -> anyhow::Result<PathBuf> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest)?;
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path)?).context("the download isn't a zip")?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        // Paths that would land outside `dest` are refused (enclosed_name).
        let Some(rel) = f.enclosed_name() else { bail!("the zip has a bad path: {:?}", f.name()) };
        let out = dest.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(dir) = out.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::io::copy(&mut f, &mut std::fs::File::create(&out)?)?;
        }
    }
    let root = dest.join(PACKAGE_DIR);
    anyhow::ensure!(root.join("photo-stamp.exe").is_file(), "the download doesn't contain photo-stamp.exe");
    Ok(root)
}

impl Updater {
    pub fn new(geo: Arc<RemoteGeocoder>, status: Arc<Status>, data: &Path, program_dir: &Path) -> Arc<Self> {
        let unsupported = if !cfg!(windows) {
            Some("updates are installed with the Docker image on this system".to_owned())
        } else if !program_dir.join(crate::outbox::SUPPORT_DIR).is_dir() {
            Some("this copy isn't a packaged install (no photo-stamp-files folder)".to_owned())
        } else {
            None
        };
        let me = Arc::new(Self {
            geo,
            status,
            dir: data.join("updates"),
            program_dir: program_dir.to_owned(),
            state: Mutex::new(State::Idle),
            busy: Mutex::new(()),
            install_requested: AtomicBool::new(false),
            unsupported,
        });
        me.publish();
        me
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap() = s;
        self.publish();
    }

    fn publish(&self) {
        let state = match &self.unsupported {
            Some(r) => State::Off { reason: r.clone() },
            None => self.state.lock().unwrap().clone(),
        };
        self.status.set_update(json!({ "current": photo_stamp_http::VERSION, "info": state }));
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("install.json")
    }

    /// Ask GitHub; download and unpack a newer release. Returns the state after.
    pub fn check(&self) -> State {
        if self.unsupported.is_some() {
            self.publish();
            return self.state();
        }
        let Ok(_one) = self.busy.try_lock() else { return self.state() };
        if matches!(self.state(), State::Installing { .. }) {
            return self.state();
        }
        self.set(State::Checking);
        let result = (|| -> anyhow::Result<State> {
            let current = Version::parse(photo_stamp_http::VERSION).context("this build has no version")?;
            let Some(update) = latest_release()? else {
                return Ok(State::UpToDate { checked: now_ms() });
            };
            let newest = Version::parse(&update.version)
                .with_context(|| format!("the latest release ({}) isn't a version", update.version))?;
            if newest <= current {
                return Ok(State::UpToDate { checked: now_ms() });
            }
            let unpacked = self.dir.join(&update.version);
            if unpacked.join(PACKAGE_DIR).join("photo-stamp.exe").is_file()
                && unpacked.join(".verified").is_file()
            {
                return Ok(State::Ready { version: update.version });
            }
            std::fs::create_dir_all(&self.dir)?;
            // Earlier downloads aren't needed any more.
            if let Ok(rd) = std::fs::read_dir(&self.dir) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy() != "install.json" {
                        let _ = std::fs::remove_dir_all(e.path());
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
            log::info!("downloading photo-stamp {}", update.version);
            let zip = self.dir.join(format!("{}.zip", update.version));
            let version = update.version.clone();
            let mut last = 0u64;
            let sha = download(&update.url, &zip, &mut |done, total| {
                // Tell the dashboard every few MB.
                if done - last >= 4 << 20 || Some(done) == total {
                    last = done;
                    self.set(State::Downloading { version: version.clone(), done, total });
                }
            })?;
            if !sha.eq_ignore_ascii_case(&update.sha256) {
                let _ = std::fs::remove_file(&zip);
                bail!(
                    "the download of {} was damaged (checksum mismatch); it'll be tried again",
                    update.version
                );
            }
            unpack(&zip, &unpacked)?;
            let _ = std::fs::remove_file(&zip);
            std::fs::write(unpacked.join(".verified"), &sha)?;
            log::info!("photo-stamp {} is ready to install (from the dashboard)", update.version);
            Ok(State::Ready { version: update.version })
        })();
        let state = result.unwrap_or_else(|e| {
            log::warn!("checking for updates: {e:#}");
            State::Failed { error: format!("{e:#}"), checked: now_ms() }
        });
        self.set(state.clone());
        state
    }

    /// Check now and every [`CHECK_EVERY`], in the background.
    pub fn check_periodically(self: &Arc<Self>, first_after: Duration) {
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(first_after);
            loop {
                me.check();
                std::thread::sleep(CHECK_EVERY);
            }
        });
    }

    /// Ask for the downloaded build to be installed: the watcher should stop after the photo in
    /// hand, then [`Self::take_install`] does it.
    pub fn request_install(&self) -> anyhow::Result<String> {
        if let Some(r) = &self.unsupported {
            bail!("{r}");
        }
        let State::Ready { version } = self.state() else { bail!("no update is ready to install") };
        self.install_requested.store(true, Ordering::SeqCst);
        self.set(State::Installing { version: version.clone() });
        log::info!("installing photo-stamp {version}: finishing the photo in hand first");
        Ok(version)
    }

    pub fn install_requested(&self) -> bool {
        self.install_requested.load(Ordering::SeqCst)
    }

    /// Swap the new files in (the watcher has stopped). Returns what was installed.
    pub fn take_install(&self) -> anyhow::Result<Install> {
        let State::Installing { version } = self.state() else { bail!("no install was asked for") };
        let staged = self.dir.join(&version).join(PACKAGE_DIR);
        // The towns database is one of the files; let go of it.
        self.geo.close_towns();
        let files = match swap_in(&staged, &self.program_dir) {
            Ok(f) => f,
            Err(e) => {
                self.geo.open_towns();
                self.install_requested.store(false, Ordering::SeqCst);
                self.set(State::Failed { error: format!("{e:#}"), checked: now_ms() });
                return Err(e);
            }
        };
        let install =
            Install { from: photo_stamp_http::VERSION.to_owned(), to: version, files, rolled_back: false };
        std::fs::write(self.manifest_path(), serde_json::to_vec_pretty(&install)?)?;
        Ok(install)
    }

    /// The new version didn't come up: put the old files back and note it for the dashboard.
    pub fn roll_back(&self, mut install: Install) {
        roll_back(&self.program_dir, &install.files);
        install.rolled_back = true;
        let _ = std::fs::write(self.manifest_path(), serde_json::to_vec_pretty(&install).unwrap_or_default());
    }

    /// At startup: finish (or report) an install from the previous run.
    pub fn after_restart(self: &Arc<Self>) {
        let Ok(bytes) = std::fs::read(self.manifest_path()) else { return };
        let Ok(install) = serde_json::from_slice::<Install>(&bytes) else {
            let _ = std::fs::remove_file(self.manifest_path());
            return;
        };
        if install.rolled_back {
            log::warn!("photo-stamp {} didn't start, so {} was put back", install.to, install.from);
            self.set(State::Failed {
                error: format!("version {} didn't start, so {} was put back", install.to, install.from),
                checked: now_ms(),
            });
            let _ = std::fs::remove_file(self.manifest_path());
            let _ = std::fs::remove_dir_all(self.dir.join(&install.to));
            return;
        }
        if install.to != photo_stamp_http::VERSION {
            return;
        }
        log::info!("updated from {} to {}", install.from, install.to);
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(SETTLED_AFTER);
            settle(&me.program_dir, &install.files);
            let _ = std::fs::remove_dir_all(me.dir.join(&install.to));
            let _ = std::fs::remove_file(me.manifest_path());
            log::info!("update to {} settled; old files removed", install.to);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stamp::tests::temp_dir;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn reads_a_github_release() {
        let release: Release = serde_json::from_str(
            r#"{"tag_name": "v0.8.0", "prerelease": false, "assets": [
                {"name": "photo-stamp-setup-v0.8.0.exe", "browser_download_url": "https://x/setup.exe",
                 "digest": "sha256:dd78"},
                {"name": "photo-stamp-windows-v0.8.0.zip", "browser_download_url": "https://x/w.zip",
                 "digest": "sha256:224c"}]}"#,
        )
        .unwrap();
        assert_eq!(
            release.offer().unwrap(),
            Offer { version: "0.8.0".into(), url: "https://x/w.zip".into(), sha256: "224c".into() }
        );
        let unchecked: Release = serde_json::from_str(
            r#"{"tag_name": "v0.9.0", "assets": [{"name": "photo-stamp-windows-v0.9.0.zip",
                "browser_download_url": "https://x/w.zip"}]}"#,
        )
        .unwrap();
        assert!(unchecked.offer().unwrap_err().to_string().contains("checksum"));
        let no_zip: Release = serde_json::from_str(r#"{"tag_name": "v1.0.0"}"#).unwrap();
        assert!(no_zip.offer().is_err());
    }

    #[test]
    fn swaps_files_in_and_back() {
        let root = temp_dir("update-swap");
        let (staged, program) = (root.join("staged"), root.join("program"));
        write(&staged.join("photo-stamp.exe"), "new exe");
        write(&staged.join("photo-stamp-files/towns.sqlite"), "new towns");
        write(&staged.join("photo-stamp-files/added.txt"), "added");
        write(&program.join("photo-stamp.exe"), "old exe");
        write(&program.join("photo-stamp-files/towns.sqlite"), "old towns");
        write(&program.join("outbox/photo.jpg"), "a photo");

        let files = swap_in(&staged, &program).unwrap();
        assert_eq!(
            files,
            ["photo-stamp-files/added.txt", "photo-stamp-files/towns.sqlite", "photo-stamp.exe"]
        );
        assert_eq!(std::fs::read_to_string(program.join("photo-stamp.exe")).unwrap(), "new exe");
        assert_eq!(std::fs::read_to_string(program.join("photo-stamp.exe.old")).unwrap(), "old exe");

        roll_back(&program, &files);
        assert_eq!(std::fs::read_to_string(program.join("photo-stamp.exe")).unwrap(), "old exe");
        assert_eq!(
            std::fs::read_to_string(program.join("photo-stamp-files/towns.sqlite")).unwrap(),
            "old towns"
        );
        assert!(!program.join("photo-stamp-files/added.txt").exists(), "added files go");
        assert!(!program.join("photo-stamp.exe.old").exists());

        let files = swap_in(&staged, &program).unwrap();
        settle(&program, &files);
        assert_eq!(std::fs::read_to_string(program.join("photo-stamp.exe")).unwrap(), "new exe");
        assert!(!program.join("photo-stamp.exe.old").exists(), "settled: no .old files");
        assert_eq!(
            std::fs::read_to_string(program.join("outbox/photo.jpg")).unwrap(),
            "a photo",
            "photos untouched"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unpacks_a_build_and_refuses_bad_paths() {
        use std::io::Write;
        let root = temp_dir("update-unpack");
        let zip_path = root.join("b.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("photo-stamp-windows/photo-stamp.exe", opts).unwrap();
        z.write_all(b"exe").unwrap();
        z.start_file("photo-stamp-windows/photo-stamp-files/towns.sqlite", opts).unwrap();
        z.write_all(b"towns").unwrap();
        z.finish().unwrap();
        let staged = unpack(&zip_path, &root.join("out")).unwrap();
        assert_eq!(std::fs::read(staged.join("photo-stamp-files/towns.sqlite")).unwrap(), b"towns");

        let bad = root.join("bad.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
        z.start_file("../escape.exe", opts).unwrap();
        z.write_all(b"x").unwrap();
        z.finish().unwrap();
        assert!(unpack(&bad, &root.join("out2")).is_err());
        assert!(!root.join("escape.exe").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
