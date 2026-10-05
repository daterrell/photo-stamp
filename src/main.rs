use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use photo_stamp::geo::{Geocoder, NoGeocoder, OsmGeocoder};
use photo_stamp::stamp::{Options, Outcome, process_file};

/// Burn the date and place each photo was taken into the photo itself.
///
/// JPEG files are modified IN PLACE. Photos without EXIF date/location data are left untouched,
/// as are photos that have already been stamped.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Photos, or directories to search recursively for .jpg/.jpeg files
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Show the caption each photo would get without modifying anything
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Don't contact OpenStreetMap; caption with coordinates instead of place names
    #[arg(long)]
    offline: bool,

    /// Preferred language for place names
    #[arg(long, default_value = "en")]
    lang: String,

    /// JPEG quality for re-encoded photos
    #[arg(long, default_value_t = 92, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let geo: Box<dyn Geocoder> =
        if cli.offline { Box::new(NoGeocoder) } else { Box::new(OsmGeocoder::new(&cli.lang)) };
    let opts = Options { dry_run: cli.dry_run, quality: cli.quality };

    let mut files = Vec::new();
    for p in &cli.paths {
        collect_photos(p, &mut files);
    }

    let mut failures = 0;
    for path in &files {
        let shown = path.display();
        match process_file(path, geo.as_ref(), opts) {
            Ok(Outcome::Stamped(lines)) => {
                let verb = if cli.dry_run { "would stamp" } else { "stamped" };
                println!("{verb:<12} {shown}: {}", lines.join(" | "));
            }
            Ok(Outcome::NotJpeg) => println!("{:<12} {shown}: not a JPEG", "skipped"),
            Ok(Outcome::NoExif) => println!("{:<12} {shown}: no EXIF data", "skipped"),
            Ok(Outcome::NothingToStamp) => println!("{:<12} {shown}: no date or location in EXIF", "skipped"),
            Ok(Outcome::AlreadyStamped) => println!("{:<12} {shown}: already stamped", "skipped"),
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
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(path) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(e) => {
            eprintln!("{:<12} {}: {e}", "error", path.display());
            return;
        }
    };
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            collect_photos(&entry, out);
        } else if entry
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
        {
            out.push(entry);
        }
    }
}
