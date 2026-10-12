//! The per-photo pipeline: read metadata, build the caption, draw it, write the result.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::Context;
use image::DynamicImage;

use crate::formats::{self, Kind};
use crate::geo::{Geocoder, format_coords};
use crate::localtime;
use crate::metadata::{self, PhotoInfo};
use crate::render::{Line, Renderer};

/// Why a photo was left alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Skip {
    Unsupported,
    NoMetadata,
    AlreadyStamped,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Skip::Unsupported => "not a supported photo format",
            Skip::NoMetadata => "no date or location in its metadata",
            Skip::AlreadyStamped => "already stamped",
        })
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

/// The place lookup failed (e.g. no network). Callers that can wait may retry later rather
/// than settle for coordinates.
#[derive(Debug)]
pub struct GeoUnavailable(pub anyhow::Error);

impl fmt::Display for GeoUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "place lookup failed: {:#}", self.0)
    }
}

impl std::error::Error for GeoUnavailable {}

/// Place lines (if GPS) then the time line (if any), top line first.
/// With `strict_geo`, a failed or partial lookup is an error ([`GeoUnavailable`]) rather than
/// settling for coordinates or a less specific name.
pub fn caption_lines(info: &PhotoInfo, geo: &dyn Geocoder, strict_geo: bool) -> anyhow::Result<Vec<String>> {
    let place: Vec<String> = match info.gps {
        None => Vec::new(),
        Some(at) => match geo.place(at) {
            Ok(Some(place)) if strict_geo && !place.complete => {
                return Err(GeoUnavailable(anyhow::anyhow!("venue/landmark lookup incomplete")).into());
            }
            Ok(Some(place)) => place.lines(),
            Ok(None) => vec![format_coords(at)],
            Err(e) if strict_geo => return Err(GeoUnavailable(e).into()),
            Err(e) => {
                log::warn!("place lookup failed, using coordinates: {e:#}");
                vec![format_coords(at)]
            }
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
        return Ok(Prepared::Skip(Skip::NoMetadata));
    }
    let lines = caption_lines(&info, geo, strict_geo)?;
    if lines.is_empty() {
        return Ok(Prepared::Skip(Skip::NoMetadata));
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
    use crate::metadata::GpsPoint;
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

        for (path, why) in
            [(&bare, Skip::NoMetadata), (&camera_only, Skip::NoMetadata), (&text, Skip::Unsupported)]
        {
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
