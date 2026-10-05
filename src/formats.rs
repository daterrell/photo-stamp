//! Which files we handle and how to get pixels in and out of them.

use std::ffi::OsString;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::Context;
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat};

use crate::metadata;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Read and written in place by the `image` crate.
    Raster(ImageFormat),
    /// HEIC/HEIF/AVIF via libheif.
    Heif,
    /// Camera RAW: can't be re-written, so a stamped JPEG is made from its embedded preview.
    Raw,
}

const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "iiq", "k25", "kdc", "mef", "mos", "mrw", "nef",
    "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
];

pub fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" | "jpe" => Kind::Raster(ImageFormat::Jpeg),
        "png" => Kind::Raster(ImageFormat::Png),
        "webp" => Kind::Raster(ImageFormat::WebP),
        "tif" | "tiff" => Kind::Raster(ImageFormat::Tiff),
        "heic" | "heif" | "hif" | "avif" if cfg!(feature = "heif") => Kind::Heif,
        e if RAW_EXTENSIONS.contains(&e) => Kind::Raw,
        _ => return None,
    })
}

/// File name of the stamped result: the same name, except RAW files become
/// "IMG_0001.CR3.jpg" so they never collide with a camera's own RAW+JPEG pair.
pub fn output_name(path: &Path, kind: Kind) -> OsString {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    if kind == Kind::Raw {
        name.push(".jpg");
    }
    name
}

/// Decode `path` with pixels upright as a viewer would display them.
pub fn decode(path: &Path, kind: Kind, orientation: u8) -> anyhow::Result<DynamicImage> {
    let mut img = match kind {
        Kind::Raster(fmt) => {
            let reader = image::ImageReader::with_format(std::io::BufReader::new(File::open(path)?), fmt);
            reader.decode().context("decoding image")?
        }
        Kind::Raw => {
            let preview = metadata::raw_preview(path)?;
            image::load_from_memory_with_format(&preview, ImageFormat::Jpeg)
                .context("decoding RAW preview")?
        }
        // libheif applies the container's own rotation/mirroring while decoding.
        Kind::Heif => return heif::decode(path),
    };
    if let Some(o) = Orientation::from_exif(orientation) {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// Encode `img` to `dest` in the format `kind` writes.
pub fn encode(img: &DynamicImage, kind: Kind, dest: &Path, quality: u8) -> anyhow::Result<()> {
    match kind {
        Kind::Raster(ImageFormat::Jpeg) | Kind::Raw => {
            let mut out = BufWriter::new(File::create(dest)?);
            JpegEncoder::new_with_quality(&mut out, quality).encode_image(&img.to_rgb8())?;
        }
        Kind::Raster(fmt) => {
            let mut out = BufWriter::new(File::create(dest)?);
            img.write_to(&mut out, fmt).with_context(|| format!("encoding {fmt:?}"))?;
        }
        Kind::Heif => heif::encode(img, dest, quality)?,
    }
    Ok(())
}

#[cfg(feature = "heif")]
mod heif {
    use super::*;
    use image::{RgbImage, RgbaImage};
    use libheif_rs::{
        Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, LibHeif, RgbChroma,
    };

    fn path_str(path: &Path) -> anyhow::Result<&str> {
        path.to_str().context("HEIF path is not valid UTF-8")
    }

    pub fn decode(path: &Path) -> anyhow::Result<DynamicImage> {
        let lib = LibHeif::new();
        let ctx = HeifContext::read_from_file(path_str(path)?)?;
        let handle = ctx.primary_image_handle()?;
        let alpha = handle.has_alpha_channel();
        let chroma = if alpha { RgbChroma::Rgba } else { RgbChroma::Rgb };
        let decoded = lib.decode(&handle, ColorSpace::Rgb(chroma), None)?;
        let planes = decoded.planes();
        let plane = planes.interleaved.context("libheif returned no interleaved plane")?;
        let channels = if alpha { 4 } else { 3 };
        let row = plane.width as usize * channels;
        let mut pixels = Vec::with_capacity(row * plane.height as usize);
        for y in 0..plane.height as usize {
            pixels.extend_from_slice(&plane.data[y * plane.stride..][..row]);
        }
        Ok(if alpha {
            DynamicImage::ImageRgba8(RgbaImage::from_raw(plane.width, plane.height, pixels).unwrap())
        } else {
            DynamicImage::ImageRgb8(RgbImage::from_raw(plane.width, plane.height, pixels).unwrap())
        })
    }

    pub fn encode(img: &DynamicImage, dest: &Path, quality: u8) -> anyhow::Result<()> {
        let alpha = img.color().has_alpha();
        let (w, h) = (img.width(), img.height());
        let (chroma, pixels, channels) = if alpha {
            (RgbChroma::Rgba, img.to_rgba8().into_raw(), 4)
        } else {
            (RgbChroma::Rgb, img.to_rgb8().into_raw(), 3)
        };
        let mut image = Image::new(w, h, ColorSpace::Rgb(chroma))?;
        image.create_plane(Channel::Interleaved, w, h, 8)?;
        {
            let planes = image.planes_mut();
            let plane = planes.interleaved.context("no interleaved plane")?;
            let row = w as usize * channels;
            for y in 0..h as usize {
                plane.data[y * plane.stride..][..row].copy_from_slice(&pixels[y * row..][..row]);
            }
        }
        let format = match dest.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("avif") => CompressionFormat::Av1,
            _ => CompressionFormat::Hevc,
        };
        let lib = LibHeif::new();
        let mut encoder = lib.encoder_for_format(format).context("no HEIF encoder available")?;
        encoder.set_quality(EncoderQuality::Lossy(quality))?;
        let mut ctx = HeifContext::new()?;
        ctx.encode_image(&image, &mut encoder, None)?;
        ctx.write_to_file(path_str(dest)?)?;
        Ok(())
    }
}

#[cfg(not(feature = "heif"))]
mod heif {
    use super::*;
    use anyhow::bail;

    pub fn decode(_: &Path) -> anyhow::Result<DynamicImage> {
        bail!("built without HEIF support")
    }

    pub fn encode(_: &DynamicImage, _: &Path, _: u8) -> anyhow::Result<()> {
        bail!("built without HEIF support")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension() {
        assert_eq!(classify(Path::new("a/IMG_1.JPG")), Some(Kind::Raster(ImageFormat::Jpeg)));
        assert_eq!(classify(Path::new("x.tiff")), Some(Kind::Raster(ImageFormat::Tiff)));
        assert_eq!(classify(Path::new("x.webp")), Some(Kind::Raster(ImageFormat::WebP)));
        assert_eq!(classify(Path::new("x.CR3")), Some(Kind::Raw));
        assert_eq!(classify(Path::new("x.dng")), Some(Kind::Raw));
        #[cfg(feature = "heif")]
        assert_eq!(classify(Path::new("x.HEIC")), Some(Kind::Heif));
        assert_eq!(classify(Path::new("notes.txt")), None);
        assert_eq!(classify(Path::new("noext")), None);
    }

    #[test]
    fn raw_output_names_dont_collide_with_camera_jpegs() {
        assert_eq!(output_name(Path::new("d/IMG_1.CR3"), Kind::Raw), "IMG_1.CR3.jpg");
        assert_eq!(output_name(Path::new("d/IMG_1.png"), Kind::Raster(ImageFormat::Png)), "IMG_1.png");
    }
}
