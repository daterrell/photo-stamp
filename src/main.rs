// A background app on Windows: no console window. One-shot commands (--stop, stamp, --help…)
// print to the terminal they were run from.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use log::{error, info};
use photo_stamp::appconfig::{self, AppConfig};
use photo_stamp::geo::Geocoder;
use photo_stamp::outbox::{self, Outbox};
use photo_stamp::remote::RemoteGeocoder;
use photo_stamp::render::Renderer;
use photo_stamp::settings::{self, Settings};
use photo_stamp::stamp::{Options, Outcome, stamp_in_place};
use photo_stamp::status::Status;
use photo_stamp::update::Updater;
use photo_stamp::{api, dashboard, desktop, formats, logging, metadata, paths, watch};
use photo_stamp_http::registry::Registration;

/// Burn the place and local time each photo was taken into the photo itself.
///
/// With no command, watches the drop folder (chosen in setup, or the folder this program is in)
/// and its subfolders, and moves each photo dropped there to its outbox folder, stamped, keeping
/// the subfolders. The dashboard is at http://127.0.0.1:8765/. Supports JPEG, PNG,
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
    /// Folder to watch [default: the one chosen in setup, else the one this program is in]
    #[arg(long)]
    root: Option<PathBuf>,

    /// Where finished photos go [default: the "outbox" folder inside the watched folder]
    #[arg(long)]
    outbox: Option<PathBuf>,

    /// Start without opening the dashboard (at login, after an update)
    #[arg(long)]
    background: bool,

    /// Port on 127.0.0.1 for the dashboard [default: 8765]
    #[arg(long, env = "PHOTO_STAMP_UI_PORT")]
    port: Option<u16>,

    /// Quit the running photo-stamp (it finishes the photos in hand first)
    #[arg(long, alias = "stop", conflicts_with = "configure")]
    quit: bool,

    /// Set this PC up to watch DIR and start at login (the setup program does this)
    #[arg(long, value_name = "DIR")]
    configure: Option<PathBuf>,

    /// With --configure: name of a separate install (for tests)
    #[arg(long, requires = "configure")]
    instance: Option<String>,

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

/// The key naming this install's single-instance lock and stop signal: one photo-stamp per data
/// folder (that is, per user, unless `PHOTO_STAMP_DATA_DIR` says otherwise).
fn instance_key(data: Option<&Path>, root: &Path) -> String {
    paths::folder_key(data.unwrap_or(root))
}

/// `p` as an absolute path in its familiar form (canonicalize() on Windows gives \\?\C:\…).
fn clean_path(p: &Path) -> PathBuf {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| absolute(p));
    PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\").to_owned())
}

/// The default action: stamp photos dropped in the drop folder into the outbox, and serve the
/// dashboard.
fn here(args: HereArgs, common: &Common) -> ExitCode {
    let exe = std::env::current_exe().ok();
    let data = paths::data_dir();
    if let Some(dir) = &args.configure {
        return configure(dir, args.instance.as_deref(), exe.as_deref(), data.as_deref());
    }
    let config = data.as_deref().and_then(AppConfig::load);
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let Some(root) = appconfig::choose_root(args.root.as_deref(), config.as_ref(), exe_dir) else {
        tell("Can't tell which folder to watch; use --root.", true);
        return ExitCode::FAILURE;
    };
    let root = clean_path(&root);
    let key = instance_key(data.as_deref(), &root);
    if args.quit {
        return quit(&key);
    }

    let log_file = data.as_ref().map(|d| d.join("photo-stamp.log"));
    // The watcher logs to its file (and the terminal on Linux); on Windows it never attaches to a
    // terminal, as it keeps running long after the command that started it.
    if let Err(e) = logging::init(log_file.as_deref(), cfg!(not(windows))) {
        tell(&format!("Couldn't start logging: {e:#}"), true);
    }

    // Already running: show its dashboard.
    #[cfg(windows)]
    let instance = match photo_stamp::winsys::single_instance(&key) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            if !args.background {
                open_running(data.as_deref(), &root, args.port);
            }
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
    let folder = paths::folder_key(&root);
    let settings_path = data.as_deref().map(|d| Settings::path(d, &folder));
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

    // The dashboard's address is needed early (notifications link to it).
    let listener = match bind_dashboard(args.port.unwrap_or(dashboard::DEFAULT_PORT)) {
        Ok(l) => l,
        Err(e) => {
            error!("can't serve the dashboard: {e}");
            tell(&format!("Couldn't start photo-stamp's dashboard: {e}"), true);
            return ExitCode::FAILURE;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(dashboard::DEFAULT_PORT);
    let url = dashboard::url(port);

    let geo = RemoteGeocoder::new(data.as_deref(), settings.offline);
    if geo.server().is_none() && !settings.offline {
        info!("no place server set up (dashboard: Settings > Place server); naming places by nearest town");
    }
    let status = Arc::new(Status::new(&root, &outbox, data.as_deref(), &folder, settings));
    {
        let status = status.clone();
        logging::set_sink(Box::new(move |level, text| status.log(level, text)));
    }
    #[cfg(windows)]
    {
        let url = url.clone();
        status.set_notifier(Box::new(move |title, body| {
            if let Err(e) = photo_stamp::winsys::toast(title, body, &url) {
                log::warn!("couldn't show a notification: {e:#}");
            }
        }));
    }
    #[cfg(not(windows))]
    status.set_notifier(Box::new(|title, body| info!("{title}: {body}")));

    geo.warm_up();
    // Updates (Windows installs): checked in the background, installed when asked from the dashboard.
    let updater =
        data.as_deref().zip(exe_dir).map(|(d, dir)| Updater::new(geo.clone(), status.clone(), d, dir));
    if let Some(u) = &updater {
        u.after_restart();
        u.check_periodically(Duration::from_secs(60));
    }
    #[cfg(windows)]
    if let Some(dir) = exe_dir {
        let instance = config.as_ref().and_then(|c| c.instance.as_deref());
        photo_stamp::winsys::sync_installed_version(instance, dir, photo_stamp_http::VERSION);
    }
    let config_out = outbox::Config {
        outbox: outbox.clone(),
        events: true,
        originals: Some(originals),
        waiting_file: data.as_deref().map(|d| d.join(format!("waiting-{folder}.json"))),
    };
    let watcher = match Outbox::new(&root, config_out, &geo, status.clone()) {
        Ok(w) => w,
        Err(e) => {
            error!("{e:#}");
            tell(&format!("{e:#}"), true);
            return ExitCode::FAILURE;
        }
    };
    #[cfg(windows)]
    if let Err(e) = photo_stamp::winsys::on_stop_signal(&key, watcher.waker()) {
        log::warn!("--quit won't work for this copy: {e}");
    }
    if let Some(log) = &log_file {
        info!("log file: {}", log.display());
    }

    let mut api = api::Api::new(status.clone(), watcher.waker(), &root, &outbox);
    api.settings_path = settings_path;
    api.geo = Some(geo.clone());
    api.updater = updater.clone();
    api.port.store(port, std::sync::atomic::Ordering::Relaxed);
    if cfg!(windows)
        && let Some(exe) = &exe
    {
        api.startup = Some(api::Startup {
            name: AppConfig::startup_name(config.as_ref().and_then(|c| c.instance.as_deref())),
            command: appconfig::startup_command(exe, args.root.as_deref()),
        });
    }
    let api = Arc::new(api);
    let mut server = {
        let api = api.clone();
        match photo_stamp_http::Server::spawn(listener, Arc::new(move |req| api::handle(&api, req))) {
            Ok(s) => s,
            Err(e) => {
                error!("can't serve the dashboard: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    info!("dashboard: {url}");
    let registration = register(data.as_deref(), &folder, &root, &outbox, exe.as_deref(), port);
    #[cfg(windows)]
    let tray = photo_stamp::tray::Tray::start(&key, api.clone(), &url);
    if !args.background {
        desktop::open(&url);
    }

    let result = watcher.run();

    // Let go of the dashboard's port and the tray icon first: the next copy (after an update or a
    // restart) takes them over.
    api.closing.store(true, std::sync::atomic::Ordering::Relaxed);
    server.stop();
    #[cfg(windows)]
    drop(tray);
    if let (Some(mut reg), Some(data)) = (registration, data.as_deref()) {
        reg.running = false;
        reg.port = None;
        let _ = reg.save(data);
    }
    // Stopped to install an update: hand over to the new version (and watch that it starts).
    if let Some(u) = updater.as_ref().filter(|u| u.install_requested()) {
        #[cfg(windows)]
        drop(instance);
        return hand_over(u, &folder, data.as_deref());
    }
    if api.restart.load(std::sync::atomic::Ordering::Relaxed) {
        #[cfg(windows)]
        drop(instance);
        if let Some(exe) = &exe
            && let Err(e) = spawn_detached(exe, &relaunch_args())
        {
            error!("couldn't start again: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Listen for the dashboard on 127.0.0.1:`port`. Right after an update or restart the previous
/// copy may still be letting go of it, so keep trying for a while; if it stays taken (by some
/// other program), use any free port.
fn bind_dashboard(port: u16) -> std::io::Result<std::net::TcpListener> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => return Ok(l),
            Err(e) if std::time::Instant::now() < deadline => {
                log::debug!("port {port}: {e}; trying again");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                log::warn!("port {port} is taken ({e}); serving the dashboard on another port");
                return std::net::TcpListener::bind(("127.0.0.1", 0));
            }
        }
    }
}

/// Record how to reach this copy: for a second launch (to open its dashboard) and for the update
/// watchdog (to see the new version come up).
fn register(
    data: Option<&Path>,
    folder: &str,
    root: &Path,
    outbox: &Path,
    exe: Option<&Path>,
    port: u16,
) -> Option<Registration> {
    let data = data?;
    let reg = Registration {
        key: folder.to_owned(),
        root: root.to_owned(),
        outbox: outbox.to_owned(),
        exe: exe.map(Path::to_owned),
        pid: std::process::id(),
        port: Some(port),
        token: String::new(),
        version: photo_stamp_http::VERSION.to_owned(),
        started: photo_stamp::status::now_ms(),
        running: true,
    };
    match reg.save(data) {
        Ok(()) => Some(reg),
        Err(e) => {
            log::warn!("couldn't record this copy in {}: {e}", data.display());
            None
        }
    }
}

/// Open the dashboard of the photo-stamp that's already running.
#[cfg(windows)]
fn open_running(data: Option<&Path>, root: &Path, port: Option<u16>) {
    let running = data
        .and_then(|d| Registration::load(d, &paths::folder_key(root)))
        .filter(|r| r.running)
        .and_then(|r| r.port);
    desktop::open(&dashboard::url(running.or(port).unwrap_or(dashboard::DEFAULT_PORT)));
}

/// The arguments to start this copy again with: the same, but quietly (no dashboard popping up).
fn relaunch_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if !args.iter().any(|a| a == "--background") {
        args.push("--background".into());
    }
    args
}

/// `--configure DIR`: set this PC up to watch DIR and start at login (run by the setup program).
fn configure(dir: &Path, instance: Option<&str>, exe: Option<&Path>, data: Option<&Path>) -> ExitCode {
    let result = (|| -> anyhow::Result<PathBuf> {
        let data = data.ok_or_else(|| anyhow::anyhow!("can't find photo-stamp's data folder"))?;
        std::fs::create_dir_all(dir)?;
        let root = clean_path(dir);
        AppConfig { root: root.clone(), instance: instance.map(str::to_owned) }.save(data)?;
        #[cfg(windows)]
        {
            let exe = exe.ok_or_else(|| anyhow::anyhow!("can't find this program's location"))?;
            photo_stamp::winsys::install_startup(
                &AppConfig::startup_name(instance),
                &appconfig::startup_command(exe, None),
            )?;
            if let Err(e) = photo_stamp::winsys::register_app_id() {
                log::warn!("couldn't register for notifications: {e}");
            }
        }
        #[cfg(not(windows))]
        let _ = exe;
        Ok(root)
    })();
    match result {
        Ok(root) => {
            if console() {
                println!("photo-stamp will watch {} and start at login.", root.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            tell(&format!("Couldn't set up photo-stamp: {e:#}"), true);
            ExitCode::FAILURE
        }
    }
}

/// `--quit`: ask the running photo-stamp to stop, and wait until it has (the setup program uses
/// this before replacing it). Quiet unless run from a terminal.
#[cfg(windows)]
fn quit(key: &str) -> ExitCode {
    use photo_stamp::winsys;
    let say = |text: &str| {
        if console() {
            println!("{text}");
        }
    };
    match winsys::signal_stop(key) {
        Ok(false) => {
            say("photo-stamp isn't running.");
            ExitCode::SUCCESS
        }
        Ok(true) => {
            if winsys::wait_until_gone(key, Duration::from_secs(90)) {
                say("photo-stamp has quit.");
                ExitCode::SUCCESS
            } else {
                say("Asked photo-stamp to quit, but it's still running.");
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            say(&format!("Couldn't ask photo-stamp to quit: {e}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn quit(_: &str) -> ExitCode {
    eprintln!("--quit is a Windows feature; on Linux stop it with your service manager (or Ctrl+C).");
    ExitCode::FAILURE
}

/// Install the downloaded update and start it; if it doesn't come up, go back to this version.
fn hand_over(updater: &Updater, key: &str, data: Option<&Path>) -> ExitCode {
    // A damaged new version must fail to start, not wait on an error dialog nobody sees.
    #[cfg(windows)]
    photo_stamp::winsys::no_error_dialogs();
    let args = relaunch_args();
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
