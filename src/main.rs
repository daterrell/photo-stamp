// A background app on Windows: no console window. One-shot commands (--stop, stamp, --help…)
// print to the terminal they were run from.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use log::{error, info};
use photo_stamp::gazetteer::Gazetteer;
use photo_stamp::geo::Geocoder;
use photo_stamp::outbox::{self, Outbox};
use photo_stamp::overture::{self, Overture};
use photo_stamp::places::PlacesDb;
use photo_stamp::render::Renderer;
use photo_stamp::stamp::{Options, Outcome, stamp_in_place};
use photo_stamp::{formats, logging, metadata, paths, watch};

/// Burn the place and local time each photo was taken into the photo itself.
///
/// With no command, watches the folder this program is in (and its subfolders) and moves each
/// photo dropped there to its outbox folder, stamped, keeping the subfolders. Supports JPEG, PNG,
/// WebP, TIFF, HEIC/HEIF/AVIF and camera RAW (moved with a stamped JPEG beside it). Photos without
/// date/location metadata, or already stamped, are moved unchanged.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
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
    /// Use only the bundled place data (towns, regions, countries, famous landmarks); don't download
    /// venue/landmark detail from Overture Maps
    #[arg(long, global = true, env = "PHOTO_STAMP_OFFLINE")]
    offline: bool,

    /// Quality for lossy re-encoding (JPEG, HEIC, RAW→JPEG)
    #[arg(long, global = true, default_value_t = 92, env = "PHOTO_STAMP_QUALITY",
          value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,
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

    /// Seconds a file must stay unchanged before it's stamped
    #[arg(long, default_value_t = 3)]
    settle_secs: u64,

    /// Seconds between full scans (backup for filesystem events)
    #[arg(long, default_value_t = 30)]
    poll_secs: u64,
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
    let geo: Box<dyn Geocoder> = match gazetteer(cli.common.offline) {
        Ok(g) => Box::new(g),
        Err(e) => {
            tell(&format!("{e:#}"), true);
            return ExitCode::FAILURE;
        }
    };

    match cli.command {
        None => here(cli.here, &cli.common, geo.as_ref()),
        Some(command) => {
            console();
            let _ = logging::init(None, true);
            if let Err(e) = metadata::check_exiftool() {
                eprintln!("error: {e:#}");
                return ExitCode::FAILURE;
            }
            match command {
                Command::Stamp { paths, dry_run } => {
                    stamp_paths(&paths, geo.as_ref(), Options { dry_run, quality: cli.common.quality })
                }
                Command::Watch { root, settle_secs, poll_secs, no_events } => {
                    let config = watch::Config {
                        settle: Duration::from_secs(settle_secs),
                        poll_interval: Duration::from_secs(poll_secs.max(1)),
                        events: !no_events,
                        quality: cli.common.quality,
                    };
                    geo.warm_up();
                    match watch::Inbox::new(&root, config, geo.as_ref()).and_then(|inbox| inbox.run()) {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            eprintln!("error: {e:#}");
                            ExitCode::FAILURE
                        }
                    }
                }
            }
        }
    }
}

/// Bundled place data, plus Overture Maps on demand unless `offline`.
fn gazetteer(offline: bool) -> anyhow::Result<Gazetteer> {
    let places = PlacesDb::open_default()?;
    let overture = if offline {
        None
    } else {
        let dir =
            overture::default_dir().ok_or_else(|| anyhow::anyhow!("no folder for the Overture cache"))?;
        Some(Overture::new(&dir)?)
    };
    Ok(Gazetteer::new(places, overture))
}

/// `p` made absolute against the current folder (it may be used later from elsewhere, e.g. at login).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_owned())
}

/// The default action: stamp photos dropped in this program's folder (or `--root`) into the outbox.
fn here(args: HereArgs, common: &Common, geo: &dyn Geocoder) -> ExitCode {
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
    let _instance = match photo_stamp::winsys::single_instance(&root) {
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

    let outbox = args.outbox.clone().map_or_else(|| root.join(outbox::OUTBOX_DIR), |o| absolute(&o));
    let config = outbox::Config {
        outbox,
        settle: Duration::from_secs(args.settle_secs),
        poll_interval: Duration::from_secs(args.poll_secs.max(1)),
        events: true,
        quality: common.quality,
    };
    geo.warm_up();
    let watcher = match Outbox::new(&root, config, geo) {
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
    match watcher.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
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
