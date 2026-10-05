//! Drawing legible text onto arbitrary photo backgrounds.
//!
//! White text over a dark outline (dilated glyph mask) plus a soft blurred drop shadow:
//! the outline keeps edges crisp on bright skies/snow, the shadow lifts text off busy detail,
//! and white fill carries on dark scenes.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{GrayImage, Luma, RgbImage};
use imageproc::drawing::{draw_text_mut, text_size};
use imageproc::filter::gaussian_blur_f32;
use imageproc::morphology::{Mask, grayscale_dilate};

static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

pub fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("embedded font is valid")
}

/// A line of text and its size relative to the base size.
pub struct Line<'a> {
    pub text: &'a str,
    pub rel_size: f32,
}

/// Burn `lines` into the bottom-left corner of `img`, first line on top.
pub fn stamp(img: &mut RgbImage, lines: &[Line], font: &FontRef) {
    if lines.is_empty() {
        return;
    }
    let (w, h) = img.dimensions();
    let short = w.min(h) as f32;
    let margin = (short * 0.035).round().max(4.0);

    // Base text height ~4% of the short side, shrunk if the widest line won't fit.
    let mut base = (short * 0.04).max(12.0);
    let widest = lines
        .iter()
        .map(|l| text_size(PxScale::from(base * l.rel_size), font, l.text).0 as f32)
        .fold(0.0, f32::max);
    let avail = w as f32 - 2.0 * margin;
    if widest > avail {
        base *= avail / widest;
    }

    let outline = (base * 0.09).max(1.0);
    let blur_sigma = (base * 0.15).max(1.0);
    let shadow_offset = (base * 0.06).round() as i64;
    let pad = (outline + 3.0 * blur_sigma).ceil() as i64 + shadow_offset;

    // Lay out lines bottom-up from the bottom margin.
    let mut placed = Vec::with_capacity(lines.len());
    let mut bottom = h as f32 - margin;
    for line in lines.iter().rev() {
        let scale = PxScale::from(base * line.rel_size);
        let scaled = font.as_scaled(scale);
        let line_h = scaled.ascent() - scaled.descent();
        let top = bottom - line_h;
        placed.push((line, scale, top));
        bottom = top - line_h * 0.15;
    }

    // Work in a local region around the text only; full-size masks on 50 MP photos are slow.
    let text_w = placed.iter().map(|(l, s, _)| text_size(*s, font, l.text).0).max().unwrap_or(0);
    let top = placed.iter().map(|p| p.2).fold(f32::MAX, f32::min);
    let rx0 = (margin as i64 - pad).max(0);
    let ry0 = (top as i64 - pad).max(0);
    let rx1 = (margin as i64 + text_w as i64 + pad).min(w as i64);
    let ry1 = h as i64;
    let (rw, rh) = ((rx1 - rx0) as u32, (ry1 - ry0) as u32);
    if rw == 0 || rh == 0 {
        return;
    }

    let mut fill = GrayImage::new(rw, rh);
    for (line, scale, line_top) in &placed {
        let x = (margin as i64 - rx0) as i32;
        let y = (*line_top as i64 - ry0) as i32;
        draw_text_mut(&mut fill, Luma([255]), x, y, *scale, font, line.text);
    }

    let outline_mask = grayscale_dilate(&fill, &Mask::disk(outline.round().clamp(1.0, 255.0) as u8));
    let shadow = gaussian_blur_f32(&outline_mask, blur_sigma);

    for ly in 0..rh {
        for lx in 0..rw {
            let sx = lx as i64 - shadow_offset;
            let sy = ly as i64 - shadow_offset;
            let shadow_a = if sx >= 0 && sy >= 0 && (sx as u32) < rw && (sy as u32) < rh {
                shadow.get_pixel(sx as u32, sy as u32)[0] as f32 / 255.0 * 0.6
            } else {
                0.0
            };
            let outline_a = outline_mask.get_pixel(lx, ly)[0] as f32 / 255.0 * 0.85;
            let fill_a = fill.get_pixel(lx, ly)[0] as f32 / 255.0;
            if shadow_a == 0.0 && outline_a == 0.0 && fill_a == 0.0 {
                continue;
            }
            let px = img.get_pixel_mut(rx0 as u32 + lx, ry0 as u32 + ly);
            for c in px.0.iter_mut() {
                let mut v = *c as f32;
                v -= v * shadow_a;
                v += (16.0 - v) * outline_a;
                v += (255.0 - v) * fill_a;
                *c = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn changed_bbox(before: &RgbImage, after: &RgbImage) -> Option<(u32, u32, u32, u32)> {
        let mut bb: Option<(u32, u32, u32, u32)> = None;
        for (x, y, p) in after.enumerate_pixels() {
            if p != before.get_pixel(x, y) {
                bb = Some(match bb {
                    None => (x, y, x, y),
                    Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x), d.max(y)),
                });
            }
        }
        bb
    }

    #[test]
    fn text_lands_bottom_left_and_inside_image() {
        let font = font();
        for (w, h) in [(800, 600), (600, 800), (64, 48)] {
            let before = RgbImage::from_pixel(w, h, Rgb([120, 160, 200]));
            let mut after = before.clone();
            let lines = [
                Line { text: "Eiffel Tower, Paris", rel_size: 1.0 },
                Line { text: "Wed 12 Jun 2024, 14:32", rel_size: 0.8 },
            ];
            stamp(&mut after, &lines, &font);
            let (x0, y0, x1, _) = changed_bbox(&before, &after).expect("something drawn");
            assert!(x0 < w / 8, "{w}x{h}: left edge {x0}");
            assert!(y0 > h / 2, "{w}x{h}: top edge {y0}");
            assert!(x1 < w, "{w}x{h}");
        }
    }

    #[test]
    fn legible_on_white_and_black() {
        // A near-white and a near-black pixel must both appear whatever the background:
        // that's the fill/outline contrast pair.
        let font = font();
        for bg in [0u8, 255] {
            let mut img = RgbImage::from_pixel(400, 300, Rgb([bg, bg, bg]));
            stamp(&mut img, &[Line { text: "Hello", rel_size: 1.0 }], &font);
            assert!(img.pixels().any(|p| p[0] > 240), "bg {bg}: no light text");
            assert!(img.pixels().any(|p| p[0] < 40), "bg {bg}: no dark outline");
        }
    }

    #[test]
    fn long_text_shrinks_to_fit() {
        let font = font();
        let before = RgbImage::from_pixel(300, 300, Rgb([90, 90, 90]));
        let mut after = before.clone();
        let long = "A Very Long Landmark Name That Would Never Fit, Some Town";
        stamp(&mut after, &[Line { text: long, rel_size: 1.0 }], &font);
        let (_, _, x1, _) = changed_bbox(&before, &after).unwrap();
        assert!(x1 < 300 - 5, "text ran off the edge: {x1}");
    }
}
