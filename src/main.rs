use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use photo_stamp::geo::{Geocoder, NoGeocoder, OsmGeocoder};
use photo_stamp::render::Renderer;
use photo_stamp::stamp::{Options, Outcome, stamp_in_place};
use photo_stamp::{formats, metadata, watch};

/// Burn the place and local time each photo was taken into the photo itself.
///
/// Supports JPEG, PNG, WebP, TIFF, HEIC/HEIF/AVIF and camera RAW (via its embedded preview).
/// Photos without date/location metadata are never touched.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    common: Common,
}

#[derive(Args)]
struct Common {
    /// Don't contact OpenStreetMap; caption with coordinates instead of place names
    #[arg(long, global = true, env = "PHOTO_STAMP_OFFLINE")]
    offline: bool,

    /// Language for place names (local names are added in parentheses)
    #[arg(long, global = true, default_value = "en", env = "PHOTO_STAMP_LANG")]
    lang: String,

    /// Quality for lossy re-encoding (JPEG, HEIC, RAW→JPEG)
    #[arg(long, global = true, default_value_t = 92, env = "PHOTO_STAMP_QUALITY",
          value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,
}

#[derive(Subcommand)]
enum Command {
    /// Stamp photos IN PLACE (RAW files get a stamped "<name>.jpg" alongside instead)
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

        /// Poll for changes instead of using filesystem events (for network/Windows mounts)
        #[arg(long, env = "PHOTO_STAMP_POLL")]
        poll: bool,

        /// Seconds a file must stay unchanged before it's processed
        #[arg(long, default_value_t = 3, env = "PHOTO_STAMP_SETTLE_SECS")]
        settle_secs: u64,

        /// Seconds between full rescans of the inbox
        #[arg(long, default_value_t = 30, env = "PHOTO_STAMP_RESCAN_SECS")]
        rescan_secs: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = metadata::check_exiftool() {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    let geo: Box<dyn Geocoder> =
        if cli.common.offline { Box::new(NoGeocoder) } else { Box::new(OsmGeocoder::new(&cli.common.lang)) };

    match cli.command {
        Command::Stamp { paths, dry_run } => {
            stamp_paths(&paths, geo.as_ref(), Options { dry_run, quality: cli.common.quality })
        }
        Command::Watch { root, poll, settle_secs, rescan_secs } => {
            let config = watch::Config {
                settle: Duration::from_secs(settle_secs),
                rescan: Duration::from_secs(rescan_secs.max(1)),
                poll,
                quality: cli.common.quality,
            };
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
