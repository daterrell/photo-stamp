//! Photo metadata via `exiftool`, which understands EXIF/XMP in every format we handle
//! (JPEG, PNG, WebP, TIFF, HEIC and camera RAW files alike).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use serde_json::Value;

/// XMP keyword added to every stamped photo so it is never stamped twice.
pub const MARKER: &str = "photo-stamp";

/// What the photo tells us about when and where it was taken.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoInfo {
    /// Camera clock reading when the photo was taken (no zone attached).
    pub taken: Option<NaiveDateTime>,
    /// The camera clock's UTC offset, if it recorded one (`OffsetTimeOriginal`).
    pub offset: Option<FixedOffset>,
    /// Satellite time of the GPS fix, always UTC.
    pub gps_time: Option<DateTime<Utc>>,
    pub gps: Option<GpsPoint>,
    /// EXIF orientation (1..=8); 1 when absent.
    pub orientation: u8,
    pub already_stamped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

impl PhotoInfo {
    /// True when there is nothing worth burning in.
    pub fn is_empty(&self) -> bool {
        self.taken.is_none() && self.gps_time.is_none() && self.gps.is_none()
    }
}

/// The bundled exiftool (`photo-stamp-files/exiftool[.exe]` beside our own executable), or the
/// one on `PATH`.
fn exiftool_program() -> PathBuf {
    let name = if cfg!(windows) { "exiftool.exe" } else { "exiftool" };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(crate::outbox::SUPPORT_DIR).join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("exiftool"))
}

/// Fail early with a helpful message if exiftool is missing.
pub fn check_exiftool() -> anyhow::Result<()> {
    exiftool(&["-ver".as_ref()]).map(drop).context(if cfg!(windows) {
        "exiftool not found: keep the photo-stamp-files folder next to photo-stamp.exe"
    } else {
        "exiftool not found: install it (e.g. `apt install libimage-exiftool-perl`) or use the Docker image"
    })
}

fn exiftool(args: &[&OsStr]) -> anyhow::Result<Vec<u8>> {
    let out = run_exiftool(args).context("running exiftool")?;
    if !out.status.success() {
        bail!("exiftool failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

#[cfg(not(windows))]
fn run_exiftool(args: &[&OsStr]) -> anyhow::Result<std::process::Output> {
    Ok(Command::new(exiftool_program()).args(args).output()?)
}

/// Windows exiftool reads its command line in the ANSI code page, which mangles file names like
/// "Café.jpg" or "東京.jpg". Arguments go through a UTF-8 argument file instead, and no console
/// window is flashed up for each call.
#[cfg(windows)]
fn run_exiftool(args: &[&OsStr]) -> anyhow::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let mut lines = String::new();
    for a in args {
        let a = a.to_str().context("argument isn't valid Unicode")?;
        anyhow::ensure!(!a.contains(['\n', '\r']), "argument contains a line break");
        lines.push_str(a);
        lines.push('\n');
    }
    let argfile = std::env::temp_dir().join(format!(
        "photo-stamp-args-{}-{}.txt",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&argfile, lines)?;
    let out = Command::new(exiftool_program())
        .args(["-charset".as_ref(), "filename=utf8".as_ref(), "-@".as_ref(), argfile.as_os_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&argfile);
    Ok(out?)
}

/// Read the metadata we care about from any supported file.
pub fn read(path: &Path) -> anyhow::Result<PhotoInfo> {
    let tags = [
        "-DateTimeOriginal",
        "-CreateDate",
        "-OffsetTimeOriginal",
        "-OffsetTime",
        "-Composite:GPSLatitude",
        "-Composite:GPSLongitude",
        "-Composite:GPSDateTime",
        "-Orientation",
        "-XMP-dc:Subject",
    ];
    let mut args: Vec<&OsStr> = ["-json", "-n"].iter().chain(&tags).map(|s| s.as_ref()).collect();
    args.extend(["--".as_ref(), path.as_os_str()]);
    parse_json(&String::from_utf8_lossy(&exiftool(&args)?))
}

/// Parse `exiftool -json -n` output for a single file.
pub fn parse_json(json: &str) -> anyhow::Result<PhotoInfo> {
    let all: Value = serde_json::from_str(json).context("parsing exiftool output")?;
    let tags = &all[0];
    let text = |k: &str| tags[k].as_str().map(str::trim).filter(|s| !s.is_empty());

    let (taken, embedded_offset) =
        text("DateTimeOriginal").or_else(|| text("CreateDate")).map(parse_exif_datetime).unwrap_or_default();
    let offset =
        text("OffsetTimeOriginal").or_else(|| text("OffsetTime")).and_then(parse_offset).or(embedded_offset);
    let gps_time = text("GPSDateTime").and_then(|s| {
        let (naive, _) = parse_exif_datetime(s.trim_end_matches('Z'));
        naive.map(|n| n.and_utc())
    });

    let gps = match (tags["GPSLatitude"].as_f64(), tags["GPSLongitude"].as_f64()) {
        // Exactly 0,0 is what many cameras write when they have no fix.
        (Some(lat), Some(lon)) if lat.abs() <= 90.0 && lon.abs() <= 180.0 && (lat, lon) != (0.0, 0.0) => {
            Some(GpsPoint { lat, lon })
        }
        _ => None,
    };

    let orientation = tags["Orientation"].as_u64().filter(|o| (1..=8).contains(o)).map_or(1, |o| o as u8);

    let already_stamped = match &tags["Subject"] {
        Value::String(s) => s == MARKER,
        Value::Array(items) => items.iter().any(|v| v.as_str() == Some(MARKER)),
        _ => false,
    };

    Ok(PhotoInfo { taken, offset, gps_time, gps, orientation, already_stamped })
}

/// "2024:06:12 14:32:05", optionally followed by fractional seconds and/or "+02:00" (XMP style).
fn parse_exif_datetime(s: &str) -> (Option<NaiveDateTime>, Option<FixedOffset>) {
    let Some(head) = s.get(..19) else { return (None, None) };
    let naive = NaiveDateTime::parse_from_str(head, "%Y:%m:%d %H:%M:%S").ok();
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    (naive, parse_offset(rest))
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    if s == "Z" {
        return FixedOffset::east_opt(0);
    }
    let sign = match s.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let (h, m) = s[1..].split_once(':')?;
    let secs = h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60;
    FixedOffset::east_opt(sign * secs)
}

/// Copy all metadata from `src` onto the freshly encoded `dest`, describing its new pixels:
/// upright orientation, new dimensions, no stale thumbnail, plus our marker keyword.
pub fn copy_to_stamped(src: &Path, dest: &Path, width: u32, height: u32) -> anyhow::Result<()> {
    let w = format!("-ExifImageWidth={width}");
    let h = format!("-ExifImageHeight={height}");
    let marker = format!("-XMP-dc:Subject+={MARKER}");
    let args: [&OsStr; 13] = [
        "-q".as_ref(),
        "-overwrite_original".as_ref(),
        "-TagsFromFile".as_ref(),
        src.as_os_str(),
        "-all:all".as_ref(),
        "-unsafe".as_ref(),
        "-icc_profile".as_ref(),
        "-Orientation#=1".as_ref(),
        "-ThumbnailImage=".as_ref(),
        w.as_ref(),
        h.as_ref(),
        marker.as_ref(),
        dest.as_os_str(),
    ];
    exiftool(&args).map(drop)
}

/// The biggest JPEG preview embedded in a camera RAW file.
pub fn raw_preview(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut best: Vec<u8> = Vec::new();
    for tag in ["-JpgFromRaw", "-PreviewImage", "-OtherImage"] {
        if let Ok(bytes) = exiftool(&[tag.as_ref(), "-b".as_ref(), "--".as_ref(), path.as_os_str()])
            && bytes.len() > best.len()
        {
            best = bytes;
        }
    }
    if best.is_empty() {
        bail!("no embedded preview image found in RAW file");
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_phone_style_metadata() {
        let info = parse_json(
            r#"[{"SourceFile":"x.heic","DateTimeOriginal":"2024:06:12 14:32:05","OffsetTimeOriginal":"+02:00",
                 "GPSLatitude":48.8584,"GPSLongitude":2.2945,"GPSDateTime":"2024:06:12 12:31:59Z",
                 "Orientation":6}]"#,
        )
        .unwrap();
        assert_eq!(info.taken.unwrap().to_string(), "2024-06-12 14:32:05");
        assert_eq!(info.offset.unwrap().local_minus_utc(), 7200);
        assert_eq!(info.gps_time.unwrap().to_rfc3339(), "2024-06-12T12:31:59+00:00");
        assert_eq!(info.gps, Some(GpsPoint { lat: 48.8584, lon: 2.2945 }));
        assert_eq!(info.orientation, 6);
        assert!(!info.already_stamped);
    }

    #[test]
    fn xmp_style_dates_and_fallbacks() {
        let info =
            parse_json(r#"[{"CreateDate":"2021:08:15 21:10:00.123-04:00","GPSLatitude":-33.9}]"#).unwrap();
        assert_eq!(info.taken.unwrap().to_string(), "2021-08-15 21:10:00");
        assert_eq!(info.offset.unwrap().local_minus_utc(), -4 * 3600);
        assert_eq!(info.gps, None, "latitude alone isn't a position");
        assert_eq!(info.orientation, 1);
    }

    #[test]
    fn blank_dates_and_zero_island_are_empty() {
        let info = parse_json(
            r#"[{"DateTimeOriginal":"0000:00:00 00:00:00","GPSLatitude":0,"GPSLongitude":0,"Make":"Acme"}]"#,
        )
        .unwrap();
        assert!(info.is_empty(), "{info:?}");
        assert!(parse_json("[{}]").unwrap().is_empty());
    }

    #[test]
    fn detects_marker() {
        for subject in [r#""photo-stamp""#, r#"["holiday","photo-stamp"]"#] {
            let info = parse_json(&format!(r#"[{{"Subject":{subject}}}]"#)).unwrap();
            assert!(info.already_stamped, "{subject}");
        }
        assert!(!parse_json(r#"[{"Subject":["holiday"]}]"#).unwrap().already_stamped);
    }
}
