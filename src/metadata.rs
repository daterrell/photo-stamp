//! Photo metadata via `exiftool`, which understands EXIF/XMP in every format we handle
//! (JPEG, PNG, WebP, TIFF, HEIC and camera RAW files alike).

use std::path::Path;
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

/// Fail early with a helpful message if exiftool is missing.
pub fn check_exiftool() -> anyhow::Result<()> {
    let out = Command::new("exiftool").arg("-ver").output().context(
        "exiftool not found: install it (e.g. `apt install libimage-exiftool-perl`) or use the Docker image",
    )?;
    if !out.status.success() {
        bail!("exiftool -ver failed");
    }
    Ok(())
}

fn exiftool(args: &[&std::ffi::OsStr]) -> anyhow::Result<Vec<u8>> {
    let out = Command::new("exiftool").args(args).output().context("running exiftool")?;
    if !out.status.success() {
        bail!("exiftool failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
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
    let mut args: Vec<&std::ffi::OsStr> = ["-json", "-n"].iter().chain(&tags).map(|s| s.as_ref()).collect();
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
    let args: [&std::ffi::OsStr; 13] = [
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
