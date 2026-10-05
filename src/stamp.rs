//! The per-photo pipeline: read EXIF, build the caption, draw it, write the JPEG back.

use std::fs::File;
use std::path::Path;

use anyhow::Context;
use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use img_parts::jpeg::{Jpeg, JpegSegment, markers};
use img_parts::{Bytes, ImageEXIF};

use crate::geo::{Geocoder, format_coords};
use crate::metadata::{PhotoInfo, parse_exif, reset_orientation};
use crate::render::{self, Line};

/// JPEG comment written into stamped files so re-running never stamps twice.
pub const MARKER: &[u8] = b"photo-stamp: caption burned in";
const EXIF_PREFIX: &[u8] = b"Exif\0\0";

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub dry_run: bool,
    pub quality: u8,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Caption burned in (or would be, on a dry run).
    Stamped(Vec<String>),
    NotJpeg,
    NoExif,
    /// EXIF present but no date or location in it.
    NothingToStamp,
    AlreadyStamped,
}

/// "Wed 12 Jun 2024, 14:32" and/or a place name, top line first.
pub fn caption_lines(info: &PhotoInfo, geo: &dyn Geocoder) -> Vec<String> {
    let place = info.gps.map(|at| match geo.place_name(at) {
        Ok(Some(name)) => name,
        Ok(None) => format_coords(at),
        Err(e) => {
            eprintln!("warning: place lookup failed, using coordinates: {e:#}");
            format_coords(at)
        }
    });
    let date = info.taken.map(|t| t.format("%a %-d %b %Y, %H:%M").to_string());
    place.into_iter().chain(date).collect()
}

/// Process an in-memory JPEG. Returns the outcome and, when stamped (not dry run), the new file bytes.
pub fn process_bytes(
    data: &[u8],
    geo: &dyn Geocoder,
    opts: Options,
) -> anyhow::Result<(Outcome, Option<Vec<u8>>)> {
    let Ok(original) = Jpeg::from_bytes(Bytes::copy_from_slice(data)) else {
        return Ok((Outcome::NotJpeg, None));
    };
    if original.segments().iter().any(|s| s.marker() == markers::COM && s.contents().starts_with(MARKER)) {
        return Ok((Outcome::AlreadyStamped, None));
    }
    let Some(exif) = original.exif() else { return Ok((Outcome::NoExif, None)) };
    let info = match parse_exif(&exif) {
        Ok(info) if !info.is_empty() => info,
        _ => return Ok((Outcome::NothingToStamp, None)),
    };

    let lines = caption_lines(&info, geo);
    if opts.dry_run {
        return Ok((Outcome::Stamped(lines), None));
    }

    // Rotate pixels upright first so the caption sits at the bottom-left as the viewer sees it.
    let mut img = image::load_from_memory_with_format(data, ImageFormat::Jpeg).context("decoding JPEG")?;
    if let Some(o) = Orientation::from_exif(info.orientation) {
        img.apply_orientation(o);
    }
    let mut rgb = img.into_rgb8();

    let font = render::font();
    let rel_sizes = [1.0, 0.8];
    let render_lines: Vec<Line> =
        lines.iter().zip(rel_sizes).map(|(text, rel_size)| Line { text, rel_size }).collect();
    render::stamp(&mut rgb, &render_lines, &font);

    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, opts.quality).encode_image(&rgb).context("encoding JPEG")?;

    let out = carry_over_metadata(&original, Bytes::from(encoded))?;
    Ok((Outcome::Stamped(lines), Some(out)))
}

/// Copy the original's APP1–APP15 segments (EXIF, XMP, ICC profile, IPTC…) into the freshly
/// encoded JPEG, with EXIF orientation reset since the pixels are now upright, then add our marker.
fn carry_over_metadata(original: &Jpeg, encoded: Bytes) -> anyhow::Result<Vec<u8>> {
    let mut out = Jpeg::from_bytes(encoded).context("re-reading encoded JPEG")?;
    let is_app_n = |s: &JpegSegment| (markers::APP1..=markers::APP15).contains(&s.marker());

    let mut carried: Vec<JpegSegment> = original
        .segments()
        .iter()
        .filter(|s| is_app_n(s))
        .map(|s| {
            if s.marker() == markers::APP1 && s.contents().starts_with(EXIF_PREFIX) {
                let mut contents = s.contents().to_vec();
                reset_orientation(&mut contents[EXIF_PREFIX.len()..]);
                JpegSegment::new_with_contents(markers::APP1, contents.into())
            } else {
                s.clone()
            }
        })
        .collect();
    carried.push(JpegSegment::new_with_contents(markers::COM, Bytes::from_static(MARKER)));

    let segments = out.segments_mut();
    segments.retain(|s| !is_app_n(s));
    let insert_at = segments.iter().position(|s| s.marker() != markers::APP0).unwrap_or(segments.len());
    segments.splice(insert_at..insert_at, carried);

    let mut buf = Vec::new();
    out.encoder().write_to(&mut buf)?;
    Ok(buf)
}

/// Process a file on disk, overwriting it in place (atomically, keeping permissions and mtime).
pub fn process_file(path: &Path, geo: &dyn Geocoder, opts: Options) -> anyhow::Result<Outcome> {
    let data = std::fs::read(path)?;
    let (outcome, new_bytes) = process_bytes(&data, geo, opts)?;
    let Some(new_bytes) = new_bytes else { return Ok(outcome) };

    let meta = std::fs::metadata(path)?;
    let file_name = path.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = path.with_file_name(format!(".{file_name}.photo-stamp.tmp"));
    let write = || -> anyhow::Result<()> {
        std::fs::write(&tmp, &new_bytes)?;
        std::fs::set_permissions(&tmp, meta.permissions())?;
        if let Ok(mtime) = meta.modified() {
            File::options().write(true).open(&tmp)?.set_modified(mtime)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::NoGeocoder;
    use crate::metadata::GpsPoint;
    use crate::metadata::tests::{build_exif, eiffel_fields};
    use image::{Rgb, RgbImage};

    struct FixedPlace;
    impl Geocoder for FixedPlace {
        fn place_name(&self, _: GpsPoint) -> anyhow::Result<Option<String>> {
            Ok(Some("Eiffel Tower, Paris".into()))
        }
    }

    struct FailingGeocoder;
    impl Geocoder for FailingGeocoder {
        fn place_name(&self, _: GpsPoint) -> anyhow::Result<Option<String>> {
            anyhow::bail!("network down")
        }
    }

    const OPTS: Options = Options { dry_run: false, quality: 90 };

    /// A 200x120 gradient JPEG, optionally carrying an EXIF block.
    fn jpeg(exif: Option<Vec<u8>>) -> Vec<u8> {
        let img = RgbImage::from_fn(200, 120, |x, y| Rgb([x as u8, y as u8, 128]));
        let mut buf = Vec::new();
        JpegEncoder::new_with_quality(&mut buf, 90).encode_image(&img).unwrap();
        let mut j = Jpeg::from_bytes(buf.into()).unwrap();
        j.set_exif(exif.map(Into::into));
        let mut out = Vec::new();
        j.encoder().write_to(&mut out).unwrap();
        out
    }

    #[test]
    fn stamps_rotates_and_preserves_exif() {
        let input = jpeg(Some(build_exif(&eiffel_fields(), false)));
        let (outcome, out) = process_bytes(&input, &FixedPlace, OPTS).unwrap();
        assert_eq!(
            outcome,
            Outcome::Stamped(vec!["Eiffel Tower, Paris".into(), "Wed 12 Jun 2024, 14:32".into()])
        );
        let out = out.unwrap();

        // Orientation 6 means "rotate 90° CW to view": pixels are now upright and tagged as such.
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (120, 200));
        let parsed = Jpeg::from_bytes(out.clone().into()).unwrap();
        let info = parse_exif(&parsed.exif().unwrap()).unwrap();
        assert_eq!(info.orientation, 1);
        assert!(info.taken.is_some() && info.gps.is_some());

        // Running again is a no-op.
        assert_eq!(process_bytes(&out, &FixedPlace, OPTS).unwrap(), (Outcome::AlreadyStamped, None));
    }

    #[test]
    fn no_exif_means_untouched() {
        assert_eq!(process_bytes(&jpeg(None), &FixedPlace, OPTS).unwrap(), (Outcome::NoExif, None));
    }

    #[test]
    fn exif_without_date_or_place_means_untouched() {
        use crate::metadata::tests::{ascii, field};
        let exif = build_exif(&[field(exif::Tag::Make, ascii("Acme"))], false);
        let result = process_bytes(&jpeg(Some(exif)), &FixedPlace, OPTS).unwrap();
        assert_eq!(result, (Outcome::NothingToStamp, None));
    }

    #[test]
    fn non_jpeg_is_skipped() {
        assert_eq!(process_bytes(b"\x89PNG....", &FixedPlace, OPTS).unwrap(), (Outcome::NotJpeg, None));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let input = jpeg(Some(build_exif(&eiffel_fields(), true)));
        let (outcome, out) = process_bytes(&input, &FixedPlace, Options { dry_run: true, ..OPTS }).unwrap();
        assert!(matches!(outcome, Outcome::Stamped(_)));
        assert!(out.is_none());
    }

    #[test]
    fn falls_back_to_coordinates() {
        let info = parse_exif(&build_exif(&eiffel_fields(), false)).unwrap();
        for geo in [&NoGeocoder as &dyn Geocoder, &FailingGeocoder] {
            let lines = caption_lines(&info, geo);
            assert!(lines[0].starts_with("48.858"), "{lines:?}");
            assert_eq!(lines.len(), 2);
        }
    }

    #[test]
    fn process_file_overwrites_in_place() {
        let dir = std::env::temp_dir().join(format!("photo-stamp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.jpg");
        let input = jpeg(Some(build_exif(&eiffel_fields(), false)));
        std::fs::write(&path, &input).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        let outcome = process_file(&path, &FixedPlace, OPTS).unwrap();
        assert!(matches!(outcome, Outcome::Stamped(_)));
        assert_ne!(std::fs::read(&path).unwrap(), input);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "temp file left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
