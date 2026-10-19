//! The per-photo pipeline: read metadata, build the caption, draw it, write the result.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::Context;
use image::DynamicImage;

use crate::formats::{self, Kind};
use crate::geo::{Geocoder, GpsPoint, format_coords};
use crate::localtime;
use crate::metadata::{self, PhotoInfo};
use crate::render::{Line, Renderer};

/// Why a photo was left alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Unsupported,
    /// Nothing to stamp: no date or place.
    NoMetadata(Missing),
    AlreadyStamped,
}

/// What a photo with nothing to stamp lacks, so the person can tell why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Missing {
    /// No camera metadata at all (screenshots, photos saved from messaging apps or the web).
    Everything,
    /// The camera's metadata has neither a date taken nor a GPS position.
    DateAndPlace { date_blank: bool, gps_zero: bool },
    /// Only the time of a GPS fix: no date taken and no position.
    OnlyGpsTime,
}

impl Missing {
    fn of(info: &PhotoInfo) -> Self {
        if info.gps_time.is_some() {
            Missing::OnlyGpsTime
        } else if !info.camera && !info.date_blank && !info.gps_zero {
            Missing::Everything
        } else {
            Missing::DateAndPlace { date_blank: info.date_blank, gps_zero: info.gps_zero }
        }
    }
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Missing::Everything => f.write_str(
                "no camera metadata at all (screenshots, and photos saved from messaging apps or \
                 websites, usually have none)",
            ),
            Missing::DateAndPlace { date_blank, gps_zero } => {
                let date = if date_blank {
                    "the date taken is blank (the camera's clock wasn't set)"
                } else {
                    "no date taken"
                };
                let place = if gps_zero {
                    "the GPS position is 0, 0 (no satellite fix)"
                } else {
                    "no GPS position (location may have been off)"
                };
                write!(f, "{date}, and {place}")
            }
            Missing::OnlyGpsTime => f.write_str("only the time of a GPS fix, with no date taken or position"),
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Skip::Unsupported => f.write_str("not a supported photo format"),
            Skip::NoMetadata(missing) => missing.fmt(f),
            Skip::AlreadyStamped => f.write_str("already stamped"),
        }
    }
}

/// A photo that will be stamped, and with what.
#[derive(Debug)]
pub struct Job {
    pub kind: Kind,
    pub info: PhotoInfo,
    pub lines: Vec<String>,
}

#[derive(Debug)]
pub enum Prepared {
    Skip(Skip),
    Stamp(Job),
}

/// The place lookup failed (e.g. the place server can't be reached). Callers that can wait may
/// retry later rather than settle for less.
#[derive(Debug)]
pub struct GeoUnavailable {
    pub error: anyhow::Error,
    /// Where the photo was taken.
    pub at: GpsPoint,
}

impl fmt::Display for GeoUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "place lookup failed: {:#}", self.error)
    }
}

impl std::error::Error for GeoUnavailable {}

/// Place lines (if GPS) then the time line (if any), top line first.
/// With `strict_geo`, a failed or partial lookup is an error ([`GeoUnavailable`]) rather than
/// settling for what's known locally (a remembered answer or the nearest town), or coordinates.
pub fn caption_lines(info: &PhotoInfo, geo: &dyn Geocoder, strict_geo: bool) -> anyhow::Result<Vec<String>> {
    let place: Vec<String> = match info.gps {
        None => Vec::new(),
        Some(at) => match geo.place(at) {
            Ok(Some(place)) if strict_geo && !place.complete => {
                let error = anyhow::anyhow!("venue/landmark lookup incomplete");
                return Err(GeoUnavailable { error, at }.into());
            }
            Ok(Some(place)) => place.lines(),
            Ok(None) => vec![format_coords(at)],
            Err(e) if strict_geo => return Err(GeoUnavailable { error: e, at }.into()),
            Err(e) => match geo.local_place(at) {
                Some(place) => {
                    log::info!("place lookup failed ({e:#}); using what's known on this PC");
                    place.lines()
                }
                None => {
                    log::warn!("place lookup failed, using coordinates: {e:#}");
                    vec![format_coords(at)]
                }
            },
        },
    };
    let tz = info.gps.and_then(localtime::timezone_at);
    let time = localtime::caption(info, tz);
    Ok(place.into_iter().chain(time).collect())
}

/// Work out whether and how `path` should be stamped. Never modifies anything.
pub fn prepare(path: &Path, geo: &dyn Geocoder, strict_geo: bool) -> anyhow::Result<Prepared> {
    let Some(kind) = formats::classify(path) else { return Ok(Prepared::Skip(Skip::Unsupported)) };
    let info = metadata::read(path)?;
    if info.already_stamped {
        return Ok(Prepared::Skip(Skip::AlreadyStamped));
    }
    if info.is_empty() {
        return Ok(Prepared::Skip(Skip::NoMetadata(Missing::of(&info))));
    }
    let lines = caption_lines(&info, geo, strict_geo)?;
    if lines.is_empty() {
        return Ok(Prepared::Skip(Skip::NoMetadata(Missing::of(&info))));
    }
    Ok(Prepared::Stamp(Job { kind, info, lines }))
}

/// Burn the job's caption into the pixels of `img`.
pub fn draw_caption(img: DynamicImage, lines: &[String], renderer: &mut Renderer) -> DynamicImage {
    // The main place line is largest; supporting lines are smaller.
    let rel_sizes = std::iter::once(1.0).chain(std::iter::repeat(0.75));
    let lines: Vec<Line> =
        lines.iter().zip(rel_sizes).map(|(text, rel_size)| Line { text, rel_size }).collect();
    if img.color().has_alpha() {
        let mut rgba = img.into_rgba8();
        renderer.stamp(&mut rgba, &lines);
        DynamicImage::ImageRgba8(rgba)
    } else {
        let mut rgb = img.into_rgb8();
        renderer.stamp(&mut rgb, &lines);
        DynamicImage::ImageRgb8(rgb)
    }
}

/// Write a stamped copy of `src` to `dest` (atomically via a temp file next to `dest`),
/// carrying over its metadata.
pub fn write_stamped(
    src: &Path,
    job: &Job,
    renderer: &mut Renderer,
    dest: &Path,
    quality: u8,
) -> anyhow::Result<()> {
    write_stamped_with(src, job, renderer, dest, quality, &mut |_, _| {})
}

/// Which picture [`write_stamped_with`] is showing its hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picture {
    /// Decoded and upright, before the caption.
    Original,
    /// With the caption, about to be saved.
    Stamped,
}

/// [`write_stamped`], showing `on_image` the picture before and after the caption is drawn
/// (e.g. to save previews) without decoding the photo twice.
pub fn write_stamped_with(
    src: &Path,
    job: &Job,
    renderer: &mut Renderer,
    dest: &Path,
    quality: u8,
    on_image: &mut dyn FnMut(Picture, &DynamicImage),
) -> anyhow::Result<()> {
    let img = formats::decode(src, job.kind, job.info.orientation)?;
    on_image(Picture::Original, &img);
    let img = draw_caption(img, &job.lines, renderer);
    on_image(Picture::Stamped, &img);

    // Keep the real extension last: exiftool picks the file format from it.
    let dest_name = dest.file_name().context("destination has no file name")?.to_string_lossy();
    let tmp = dest.with_file_name(format!(".photo-stamp-{}-{dest_name}", std::process::id()));
    let result = (|| {
        formats::encode(&img, job.kind, &tmp, quality)?;
        metadata::copy_to_stamped(src, &tmp, img.width(), img.height())?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dry_run: bool,
    pub quality: u8,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Caption burned in (or would be, on a dry run), written to the given path.
    Stamped {
        lines: Vec<String>,
        dest: PathBuf,
    },
    Skipped(Skip),
}

/// Stamp a file in place, keeping permissions and modification time. RAW files are never
/// modified; their stamped JPEG goes alongside ("IMG_1.CR3" → "IMG_1.CR3.jpg").
pub fn stamp_in_place(
    path: &Path,
    geo: &dyn Geocoder,
    renderer: &mut Renderer,
    opts: Options,
) -> anyhow::Result<Outcome> {
    let job = match prepare(path, geo, false)? {
        Prepared::Skip(s) => return Ok(Outcome::Skipped(s)),
        Prepared::Stamp(job) => job,
    };
    let dest = path.with_file_name(formats::output_name(path, job.kind));
    if job.kind == Kind::Raw && dest.exists() {
        return Ok(Outcome::Skipped(Skip::AlreadyStamped));
    }
    if !opts.dry_run {
        let meta = std::fs::metadata(path)?;
        write_stamped(path, &job, renderer, &dest, opts.quality)?;
        std::fs::set_permissions(&dest, meta.permissions())?;
        if let Ok(mtime) = meta.modified() {
            File::options().write(true).open(&dest)?.set_modified(mtime)?;
        }
    }
    Ok(Outcome::Stamped { lines: job.lines, dest })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::geo::{NoGeocoder, Place};
    use image::{ImageFormat, Rgb, RgbImage};
    use std::process::Command;

    fn eiffel(complete: bool) -> Place {
        Place {
            primary: "Eiffel Tower (Tour Eiffel)".into(),
            near: None,
            context: Some("Paris, France".into()),
            complete,
        }
    }

    pub struct FixedPlace;
    impl Geocoder for FixedPlace {
        fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
            Ok(Some(eiffel(true)))
        }
    }

    /// Nominatim answered but Overpass didn't.
    pub struct PartialPlace;
    impl Geocoder for PartialPlace {
        fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
            Ok(Some(Place { primary: "Gros-Caillou".into(), ..eiffel(false) }))
        }
    }

    /// The place server can't be reached, but this PC knows the town.
    pub struct OfflineWithTown;
    impl Geocoder for OfflineWithTown {
        fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
            anyhow::bail!("server unreachable")
        }
        fn local_place(&self, _: GpsPoint) -> Option<Place> {
            Some(Place {
                primary: "Paris".into(),
                near: None,
                context: Some("France".into()),
                complete: true,
            })
        }
        fn nearby_town(&self, _: GpsPoint) -> Option<String> {
            Some("Paris".into())
        }
    }

    pub struct FailingGeocoder;
    impl Geocoder for FailingGeocoder {
        fn place(&self, _: GpsPoint) -> anyhow::Result<Option<Place>> {
            anyhow::bail!("network down")
        }
    }

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("photo-stamp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub const EIFFEL_TAGS: &[&str] = &[
        "-DateTimeOriginal=2024:06:12 14:32:05",
        "-OffsetTimeOriginal=+02:00",
        "-GPSLatitude=48.8584",
        "-GPSLatitudeRef=N",
        "-GPSLongitude=2.2945",
        "-GPSLongitudeRef=E",
        "-Orientation#=6",
    ];

    /// Write a 200x120 gradient photo in the format given by `path`'s extension, tagged with exiftool.
    pub fn make_photo(path: &Path, tags: &[&str]) {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(200, 120, |x, y| Rgb([x as u8, y as u8, 128])));
        let kind = formats::classify(path).unwrap();
        formats::encode(&img, kind, path, 90).unwrap();
        if !tags.is_empty() {
            let out = Command::new("exiftool")
                .args(["-q", "-overwrite_original"])
                .args(tags)
                .arg(path)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    fn exif_tag(path: &Path, tag: &str) -> String {
        let out = Command::new("exiftool").args(["-s3", "-n", tag]).arg(path).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn stamps_every_writable_format_in_place() {
        let dir = temp_dir("formats");
        let mut renderer = Renderer::new();
        let opts = Options { dry_run: false, quality: 90 };
        let mut exts = vec!["jpg", "png", "webp", "tif"];
        if cfg!(feature = "heif") {
            exts.push("heic");
        }
        for ext in exts {
            let path = dir.join(format!("photo.{ext}"));
            make_photo(&path, EIFFEL_TAGS);
            let before = std::fs::read(&path).unwrap();

            let outcome = stamp_in_place(&path, &FixedPlace, &mut renderer, opts).unwrap();
            assert_eq!(
                outcome,
                Outcome::Stamped {
                    lines: vec![
                        "Eiffel Tower (Tour Eiffel)".into(),
                        "Paris, France".into(),
                        "Wed 12 Jun 2024, 14:32 CEST".into()
                    ],
                    dest: path.clone(),
                },
                "{ext}"
            );
            assert_ne!(std::fs::read(&path).unwrap(), before, "{ext}: file unchanged");

            // Orientation 6 = rotate 90° to view: pixels are now upright and tagged as such.
            let info = metadata::read(&path).unwrap();
            assert_eq!(info.orientation, 1, "{ext}");
            assert!(info.already_stamped, "{ext}: marker missing");
            assert!(info.taken.is_some() && info.gps.is_some(), "{ext}: metadata lost");
            // HEIF rotation lives in the container (irot), which libheif applies; per the HEIF
            // spec the EXIF Orientation tag is ignored there.
            let expected_size = if ext == "heic" { "200 120" } else { "120 200" };
            assert_eq!(exif_tag(&path, "-ImageSize"), expected_size, "{ext}: wrong rotation");

            // Running again is a no-op.
            let again = stamp_in_place(&path, &FixedPlace, &mut renderer, opts).unwrap();
            assert_eq!(again, Outcome::Skipped(Skip::AlreadyStamped), "{ext}");
        }
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')),
            "temp file left behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untouched_without_metadata() {
        let dir = temp_dir("nometa");
        let mut renderer = Renderer::new();
        let opts = Options { dry_run: false, quality: 90 };

        let bare = dir.join("bare.jpg");
        make_photo(&bare, &[]);
        let camera_only = dir.join("camera.png");
        make_photo(&camera_only, &["-Make=Acme"]);
        let text = dir.join("notes.txt");
        std::fs::write(&text, "hi").unwrap();

        let camera_no_date_or_gps = Missing::DateAndPlace { date_blank: false, gps_zero: false };
        for (path, why) in [
            (&bare, Skip::NoMetadata(Missing::Everything)),
            (&camera_only, Skip::NoMetadata(camera_no_date_or_gps)),
            (&text, Skip::Unsupported),
        ] {
            let before = std::fs::read(path).unwrap();
            assert_eq!(
                stamp_in_place(path, &FixedPlace, &mut renderer, opts).unwrap(),
                Outcome::Skipped(why)
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn says_what_is_missing() {
        let reason = |info: PhotoInfo| Skip::NoMetadata(Missing::of(&info)).to_string();
        assert!(reason(PhotoInfo::default()).starts_with("no camera metadata at all"));
        let camera = PhotoInfo { camera: true, ..Default::default() };
        assert_eq!(reason(camera.clone()), "no date taken, and no GPS position (location may have been off)");
        assert_eq!(
            reason(PhotoInfo { date_blank: true, gps_zero: true, ..camera }),
            "the date taken is blank (the camera's clock wasn't set), and the GPS position is 0, 0 (no satellite fix)"
        );
        let gps_time = Some(chrono::DateTime::from_timestamp(1_718_195_525, 0).unwrap());
        assert_eq!(
            reason(PhotoInfo { gps_time, ..Default::default() }),
            "only the time of a GPS fix, with no date taken or position"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = temp_dir("dry");
        let path = dir.join("p.jpg");
        make_photo(&path, EIFFEL_TAGS);
        let before = std::fs::read(&path).unwrap();
        let opts = Options { dry_run: true, quality: 90 };
        let outcome = stamp_in_place(&path, &FixedPlace, &mut Renderer::new(), opts).unwrap();
        assert!(matches!(outcome, Outcome::Stamped { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn geo_failures() {
        let info = PhotoInfo {
            gps: Some(GpsPoint { lat: 48.8584, lon: 2.2945 }),
            gps_time: Some(chrono::DateTime::from_timestamp(1_718_195_525, 0).unwrap()),
            ..Default::default()
        };
        for geo in [&NoGeocoder as &dyn Geocoder, &FailingGeocoder] {
            let lines = caption_lines(&info, geo, false).unwrap();
            assert_eq!(lines, ["48.85840° N, 2.29450° E", "Wed 12 Jun 2024, 14:32 CEST"]);
        }
        let err = caption_lines(&info, &FailingGeocoder, true).unwrap_err();
        assert!(err.downcast_ref::<GeoUnavailable>().is_some());

        // Not waiting any longer: what this PC knows beats coordinates.
        let lines = caption_lines(&info, &OfflineWithTown, false).unwrap();
        assert_eq!(lines, ["Paris", "France", "Wed 12 Jun 2024, 14:32 CEST"]);
        let err = caption_lines(&info, &OfflineWithTown, true).unwrap_err();
        assert_eq!(err.downcast_ref::<GeoUnavailable>().unwrap().at, info.gps.unwrap());

        // A partial answer is good enough when we can't wait, but not when we can.
        let lines = caption_lines(&info, &PartialPlace, false).unwrap();
        assert_eq!(lines, ["Gros-Caillou", "Paris, France", "Wed 12 Jun 2024, 14:32 CEST"]);
        let err = caption_lines(&info, &PartialPlace, true).unwrap_err();
        assert!(err.downcast_ref::<GeoUnavailable>().is_some());
    }

    #[test]
    fn raster_kinds_round_trip() {
        // Guard against a format silently losing its encoder.
        let dir = temp_dir("roundtrip");
        for (ext, fmt) in [("jpg", ImageFormat::Jpeg), ("png", ImageFormat::Png), ("webp", ImageFormat::WebP)]
        {
            let p = dir.join(format!("x.{ext}"));
            make_photo(&p, &[]);
            assert_eq!(
                image::ImageReader::open(&p).unwrap().with_guessed_format().unwrap().format(),
                Some(fmt)
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
