// A background app on Windows: no console window. One-shot commands (--stop, stamp, --help…)
// print to the terminal they were run from.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use log::{error, info};
use photo_stamp::geo::Geocoder;
use photo_stamp::outbox::{self, Outbox};
use photo_stamp::remote::RemoteGeocoder;
use photo_stamp::render::Renderer;
use photo_stamp::settings::{self, Settings};
use photo_stamp::stamp::{Options, Outcome, stamp_in_place};
use photo_stamp::status::Status;
use photo_stamp::update::Updater;
use photo_stamp::{api, formats, logging, metadata, paths, watch};
use photo_stamp_http::registry::Registration;

/// Burn the place and local time each photo was taken into the photo itself.
///
/// With no command, watches the folder this program is in (and its subfolders) and moves each
/// photo dropped there to its outbox folder, stamped, keeping the subfolders. Supports JPEG, PNG,
/// WebP, TIFF, HEIC/HEIF/AVIF and camera RAW (moved with a stamped JPEG beside it). Photos without
/// date/location metadata, or already stamped, are moved unchanged.
#[derive(Parser)]
#[command(version = photo_stamp_http::VERSION, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    here: HereArgs,

    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
struct Common {
    /// Don't ask the place server: name places from this PC's towns database only
    #[arg(long, global = true, env = "PHOTO_STAMP_OFFLINE")]
    offline: bool,

    /// Quality for lossy re-encoding (JPEG, HEIC, RAW→JPEG) [default: 92, or the dashboard's setting]
    #[arg(long, global = true, env = "PHOTO_STAMP_QUALITY",
          value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: Option<u8>,
}

/// Options for the default action: stamping dropped photos into the outbox.
#[derive(Args)]
struct HereArgs {
    /// Folder to watch instead of the one this program is in
    #[arg(long)]
    root: Option<PathBuf>,

    /// Where finished photos go [default: the "outbox" folder inside the watched folder]
    #[arg(long)]
    outbox: Option<PathBuf>,

    /// Stop the photo-stamp that is watching this folder
    #[arg(long, conflicts_with_all = ["install_startup", "uninstall_startup"])]
    stop: bool,

    /// Start watching this folder whenever you log in (and start now)
    #[arg(long, conflicts_with = "uninstall_startup")]
    install_startup: bool,

    /// Stop starting at login
    #[arg(long)]
    uninstall_startup: bool,

    /// Seconds a file must stay unchanged before it's stamped [default: 3, or the dashboard's setting]
    #[arg(long)]
    settle_secs: Option<u64>,

    /// Seconds between full scans, as a backup for filesystem events [default: 30, or the dashboard's
    /// setting]
    #[arg(long)]
    poll_secs: Option<u64>,
}

#[derive(Subcommand)]
enum Command {
    /// Stamp photos IN PLACE once (RAW files get a stamped "<name>.jpg" alongside instead)
    Stamp {
        /// Photos, or directories to search recursively
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Show the caption each photo would get without modifying anything
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Watch ROOT/inbox and sort photos into ROOT/stamped, ROOT/originals and ROOT/skipped
    Watch {
        /// Folder holding inbox/, stamped/, originals/ and skipped/ (created if missing)
        #[arg(env = "PHOTO_STAMP_ROOT")]
        root: PathBuf,

        /// Seconds a file must stay unchanged before it's processed
        #[arg(long, default_value_t = 3, env = "PHOTO_STAMP_SETTLE_SECS")]
        settle_secs: u64,

        /// Seconds between polls of the inbox (backup for filesystem events)
        #[arg(long, default_value_t = 5, env = "PHOTO_STAMP_POLL_SECS")]
        poll_secs: u64,

        /// Don't use filesystem events; rely on polling alone
        #[arg(long, env = "PHOTO_STAMP_NO_EVENTS")]
        no_events: bool,
    },
}

/// Whether output can be seen: always on Linux; on Windows only when started from a terminal.
/// On Windows this attaches to that terminal, so it's only called when there's something to say:
/// the background watcher itself never attaches (it would hold the terminal and print into it).
fn console() -> bool {
    static ATTACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ATTACHED.get_or_init(|| {
        #[cfg(windows)]
        return photo_stamp::winsys::attach_parent_console();
        #[cfg(not(windows))]
        true
    })
}

/// Tell the user something: on the terminal if started from one, otherwise in a message box.
fn tell(text: &str, is_error: bool) {
    #[cfg(windows)]
    if !console() {
        photo_stamp::winsys::message_box(text, is_error);
        return;
    }
    if is_error { eprintln!("{text}") } else { println!("{text}") }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            if console() {
                let _ = e.print();
            } else {
                tell(&e.to_string(), e.use_stderr());
            }
            return if e.use_stderr() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
        }
    };
    let Some(command) = cli.command else { return here(cli.here, &cli.common) };
    let geo = RemoteGeocoder::new(paths::data_dir().as_deref(), cli.common.offline);
    let quality = cli.common.quality.unwrap_or(settings::DEFAULT_QUALITY);
    console();
    let _ = logging::init(None, true);
    if let Err(e) = metadata::check_exiftool() {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    match command {
        Command::Stamp { paths, dry_run } => stamp_paths(&paths, &geo, Options { dry_run, quality }),
        Command::Watch { root, settle_secs, poll_secs, no_events } => {
            let config = watch::Config {
                settle: Duration::from_secs(settle_secs),
                poll_interval: Duration::from_secs(poll_secs.max(1)),
                events: !no_events,
                quality,
            };
            geo.warm_up();
            match watch::Inbox::new(&root, config, &geo).and_then(|inbox| inbox.run()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// `p` made absolute against the current folder (it may be used later from elsewhere, e.g. at login).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_owned())
}

/// The default action: stamp photos dropped in this program's folder (or `--root`) into the outbox.
fn here(args: HereArgs, common: &Common) -> ExitCode {
    let exe = std::env::current_exe().ok();
    let root = match args.root.clone().or_else(|| exe.as_ref().and_then(|e| e.parent().map(Path::to_owned))) {
        Some(r) => std::fs::canonicalize(&r).unwrap_or(r),
        None => {
            tell("Can't tell which folder to watch; use --root.", true);
            return ExitCode::FAILURE;
        }
    };
    // canonicalize() on Windows gives \\?\C:\…; show and store the familiar form.
    let root = PathBuf::from(root.to_string_lossy().trim_start_matches(r"\\?\").to_owned());

    if args.stop || args.install_startup || args.uninstall_startup {
        return manage(&args, &root, exe.as_deref());
    }

    let data = paths::data_dir();
    let log_file = data.as_ref().map(|d| d.join("photo-stamp.log"));
    // The watcher logs to its file (and the terminal on Linux); on Windows it never attaches to a
    // terminal, as it keeps running long after the command that started it.
    if let Err(e) = logging::init(log_file.as_deref(), cfg!(not(windows))) {
        tell(&format!("Couldn't start logging: {e:#}"), true);
    }

    #[cfg(windows)]
    let instance = match photo_stamp::winsys::single_instance(&root) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tell(&format!("photo-stamp is already watching {}.", root.display()), false);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            tell(&format!("Couldn't check for another photo-stamp: {e}"), true);
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = metadata::check_exiftool() {
        error!("{e:#}");
        tell(&format!("{e:#}"), true);
        return ExitCode::FAILURE;
    }

    // Saved settings (changed from the dashboard), with command-line options taking precedence.
    let key = paths::folder_key(&root);
    let settings_path = data.as_deref().map(|d| Settings::path(d, &key));
    let overrides = settings::Overrides {
        outbox: args.outbox.as_deref().map(absolute),
        quality: common.quality,
        settle_secs: args.settle_secs,
        poll_secs: args.poll_secs,
        offline: common.offline,
    };
    let settings = overrides.apply(settings_path.as_deref().map(Settings::load).unwrap_or_default());
    let outbox = settings.outbox.as_deref().map_or_else(|| root.join(outbox::OUTBOX_DIR), absolute);
    let originals = settings.originals.as_deref().map_or_else(|| root.join(outbox::ORIGINALS_DIR), absolute);

    let geo = RemoteGeocoder::new(data.as_deref(), settings.offline);
    if geo.server().is_none() && !settings.offline {
        info!("no place server set up (dashboard: Settings > Place server); naming places by nearest town");
    }
    let status = Arc::new(Status::new(&root, &outbox, data.as_deref(), &key, settings));
    {
        let status = status.clone();
        logging::set_sink(Box::new(move |level, text| status.log(level, text)));
    }
    #[cfg(windows)]
    status.set_notifier(Box::new(|title, body| {
        if let Err(e) = photo_stamp::winsys::toast(title, body, DASHBOARD_URL) {
            log::warn!("couldn't show a notification: {e:#}");
        }
    }));
    #[cfg(not(windows))]
    status.set_notifier(Box::new(|title, body| info!("{title}: {body}")));

    geo.warm_up();
    // Updates (Windows installs): checked in the background, installed when asked from the dashboard.
    let updater = data
        .as_deref()
        .zip(exe.as_deref().and_then(Path::parent))
        .map(|(d, dir)| Updater::new(geo.clone(), status.clone(), d, dir));
    if let Some(u) = &updater {
        u.after_restart();
        u.check_periodically(Duration::from_secs(60));
    }
    let config = outbox::Config {
        outbox: outbox.clone(),
        events: true,
        originals: Some(originals),
        waiting_file: data.as_deref().map(|d| d.join(format!("waiting-{key}.json"))),
    };
    let watcher = match Outbox::new(&root, config, &geo, status.clone()) {
        Ok(w) => w,
        Err(e) => {
            error!("{e:#}");
            tell(&format!("{e:#}"), true);
            return ExitCode::FAILURE;
        }
    };
    #[cfg(windows)]
    if let Err(e) = photo_stamp::winsys::on_stop_signal(&root, watcher.waker()) {
        log::warn!("--stop won't work for this instance: {e}");
    }
    if let Some(log) = &log_file {
        info!("log file: {}", log.display());
    }

    // The dashboard's way in.
    let registration = start_api(
        &status,
        &watcher,
        &geo,
        updater.clone(),
        settings_path,
        &root,
        &outbox,
        &key,
        exe.as_deref(),
    );
    let result = watcher.run();
    // Stopped to install an update: hand over to the new version (and watch that it starts).
    if let Some(u) = updater.as_ref().filter(|u| u.install_requested()) {
        #[cfg(windows)]
        drop(instance);
        return hand_over(u, &key, data.as_deref());
    }
    if let (Some(mut reg), Some(data)) = (registration, data.as_deref()) {
        reg.running = false;
        reg.port = None;
        let _ = reg.save(data);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Install the downloaded update and start it; if it doesn't come up, go back to this version.
fn hand_over(updater: &Updater, key: &str, data: Option<&Path>) -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let Ok(exe) = std::env::current_exe() else { return ExitCode::FAILURE };
    let install = match updater.take_install() {
        Ok(i) => i,
        Err(e) => {
            // Nothing was changed: carry on as this version.
            error!("couldn't install the update: {e:#}; restarting as {}", photo_stamp_http::VERSION);
            let _ = spawn_detached(&exe, &args);
            return ExitCode::FAILURE;
        }
    };
    info!("installed photo-stamp {}; starting it", install.to);
    let started = spawn_detached(&exe, &args);
    let came_up = match (started, data) {
        (Ok(mut child), Some(data)) => {
            let deadline = std::time::Instant::now() + photo_stamp::update::HANDOFF_WAIT;
            let ok = loop {
                if let Some(reg) = Registration::load(data, key)
                    && reg.pid == child.id()
                    && reg.running
                    && reg.version == install.to
                {
                    break true;
                }
                if matches!(child.try_wait(), Ok(Some(_))) || std::time::Instant::now() > deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(500));
            };
            if !ok {
                let _ = child.kill();
            }
            ok
        }
        (Ok(_), None) => true,
        (Err(e), _) => {
            error!("couldn't start the new version: {e}");
            false
        }
    };
    if came_up {
        info!("photo-stamp {} is running; this copy ({}) is done", install.to, photo_stamp_http::VERSION);
        restart_dashboard(exe.parent());
        return ExitCode::SUCCESS;
    }
    error!("photo-stamp {} didn't start; going back to {}", install.to, photo_stamp_http::VERSION);
    updater.roll_back(install);
    let _ = spawn_detached(&exe, &args);
    ExitCode::FAILURE
}

/// Start `exe` independently of this process (it outlives us).
fn spawn_detached(exe: &Path, args: &[std::ffi::OsString]) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// After an update: if the dashboard server is running, restart it as the new version.
fn restart_dashboard(dir: Option<&Path>) {
    let Some(dir) = dir else { return };
    let port: u16 = std::env::var("PHOTO_STAMP_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8765);
    let base = format!("http://127.0.0.1:{port}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into();
    let running = |agent: &ureq::Agent| {
        agent
            .get(&format!("{base}/api/ping"))
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_to_string().ok())
            .is_some_and(|b| b.contains("photo-stamp-ui"))
    };
    if !running(&agent) {
        return;
    }
    let _ = agent.post(&format!("{base}/api/quit")).header("Origin", &base).send_empty();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        if !running(&agent) {
            break;
        }
    }
    let ui = dir.join(if cfg!(windows) { "photo-stamp-ui.exe" } else { "photo-stamp-ui" });
    let args: [std::ffi::OsString; 1] = ["--no-open".into()];
    match spawn_detached(&ui, &args) {
        Ok(_) => info!("restarted the dashboard server as the new version"),
        Err(e) => log::warn!("couldn't restart the dashboard server: {e}"),
    }
}

/// Where the dashboard (photo-stamp-ui) serves its page.
#[cfg(windows)]
const DASHBOARD_URL: &str = "http://127.0.0.1:8765/";

/// Start the local API and record how to reach it, for the dashboard.
#[allow(clippy::too_many_arguments)]
fn start_api(
    status: &Arc<Status>,
    watcher: &Outbox,
    geo: &Arc<RemoteGeocoder>,
    updater: Option<Arc<Updater>>,
    settings_path: Option<PathBuf>,
    root: &Path,
    outbox: &Path,
    key: &str,
    exe: Option<&Path>,
) -> Option<Registration> {
    let data = paths::data_dir()?;
    let started = (|| -> anyhow::Result<Registration> {
        let token = api::new_token()?;
        let port = api::start(Arc::new(api::Api {
            status: status.clone(),
            waker: Mutex::new(watcher.waker()),
            token: token.clone(),
            settings_path,
            geo: Some(geo.clone()),
            updater,
        }))?;
        let reg = Registration {
            key: key.to_owned(),
            root: root.to_owned(),
            outbox: outbox.to_owned(),
            exe: exe.map(Path::to_owned),
            pid: std::process::id(),
            port: Some(port),
            token,
            version: photo_stamp_http::VERSION.to_owned(),
            started: photo_stamp::status::now_ms(),
            running: true,
        };
        reg.save(&data)?;
        Ok(reg)
    })();
    match started {
        Ok(reg) => Some(reg),
        Err(e) => {
            log::warn!("the dashboard won't be able to connect: {e:#}");
            None
        }
    }
}

/// --stop / --install-startup / --uninstall-startup.
#[cfg(windows)]
fn manage(args: &HereArgs, root: &Path, exe: Option<&Path>) -> ExitCode {
    use photo_stamp::winsys;
    let result: anyhow::Result<String> = (|| {
        if args.stop {
            return Ok(if winsys::signal_stop(root)? {
                format!("Asked photo-stamp to stop watching {}.", root.display())
            } else {
                format!("photo-stamp isn't running for {}.", root.display())
            });
        }
        if args.uninstall_startup {
            return Ok(if winsys::uninstall_startup(root)? {
                format!("photo-stamp will no longer start at login for {}.", root.display())
            } else {
                format!("photo-stamp wasn't set to start at login for {}.", root.display())
            });
        }
        let exe = exe.ok_or_else(|| anyhow::anyhow!("can't find this program's location"))?;
        let mut command = format!("\"{}\"", exe.display());
        if args.root.is_some() {
            command.push_str(&format!(" --root \"{}\"", root.display()));
        }
        if let Some(o) = &args.outbox {
            command.push_str(&format!(" --outbox \"{}\"", absolute(o).display()));
        }
        winsys::install_startup(root, &command)?;
        // And start it now, fully detached from this terminal (if it's already running, the new
        // copy notices and exits).
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let mut start = std::process::Command::new(exe);
        if args.root.is_some() {
            start.arg("--root").arg(root);
        }
        if let Some(o) = &args.outbox {
            start.arg("--outbox").arg(absolute(o));
        }
        start
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS)
            .spawn()?;
        Ok(format!(
            "photo-stamp will start at login and is now watching {}.\nPhotos dropped there are stamped into {}.",
            root.display(),
            args.outbox.as_deref().map_or_else(|| root.join(outbox::OUTBOX_DIR), absolute).display()
        ))
    })();
    match result {
        Ok(msg) => {
            tell(&msg, false);
            ExitCode::SUCCESS
        }
        Err(e) => {
            tell(&format!("{e:#}"), true);
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn manage(_: &HereArgs, _: &Path, _: Option<&Path>) -> ExitCode {
    eprintln!("--stop and --install-startup are Windows features; on Linux use your service manager.");
    ExitCode::FAILURE
}

fn stamp_paths(paths: &[PathBuf], geo: &dyn Geocoder, opts: Options) -> ExitCode {
    let mut files = Vec::new();
    for p in paths {
        collect_photos(p, &mut files);
    }
    let mut renderer = Renderer::new();
    let mut failures = 0;
    for path in &files {
        let shown = path.display();
        match stamp_in_place(path, geo, &mut renderer, opts) {
            Ok(Outcome::Stamped { lines, dest }) => {
                let verb = if opts.dry_run { "would stamp" } else { "stamped" };
                let to = if &dest == path { String::new() } else { format!(" → {}", dest.display()) };
                println!("{verb:<12} {shown}: {}{to}", lines.join(" | "));
            }
            Ok(Outcome::Skipped(why)) => println!("{:<12} {shown}: {why}", "skipped"),
            Err(e) => {
                failures += 1;
                eprintln!("{:<12} {shown}: {e:#}", "error");
            }
        }
    }
    if failures > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn collect_photos(path: &Path, out: &mut Vec<PathBuf>) {
    if !path.is_dir() {
        out.push(path.to_owned());
        return;
    }
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| match e {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("{:<12} {err}", "error");
                None
            }
        })
        .filter(|e| e.file_type().is_file() && formats::classify(e.path()).is_some())
        .map(|e| e.into_path())
        // Hidden files include our own in-progress temp files.
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    found.sort();
    out.extend(found);
}
