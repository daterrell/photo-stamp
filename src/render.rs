//! Drawing legible text onto arbitrary photo backgrounds.
//!
//! White text over a dark outline (dilated glyph mask) plus a soft blurred drop shadow:
//! the outline keeps edges crisp on bright skies/snow, the shadow lifts text off busy detail,
//! and white fill carries on dark scenes.
//!
//! Text is shaped with cosmic-text so local place names in any script (CJK, Arabic, Thai,
//! Devanagari…) render correctly, falling back from the embedded font to system fonts per glyph.

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use image::{GrayImage, ImageBuffer, Pixel};
use imageproc::filter::gaussian_blur_f32;
use imageproc::morphology::{Mask, grayscale_dilate};

static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");
const FAMILY: &str = "DejaVu Sans";
const LINE_SPACING: f32 = 1.25;

/// A line of text and its size relative to the base size.
pub struct Line<'a> {
    pub text: &'a str,
    pub rel_size: f32,
}

/// Owns the font database (slow to build, so make one and reuse it).
pub struct Renderer {
    fonts: FontSystem,
    cache: SwashCache,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        let mut fonts = FontSystem::new();
        fonts.db_mut().load_font_data(FONT_BYTES.to_vec());
        Self { fonts, cache: SwashCache::new() }
    }

    fn layout(&mut self, text: &str, size: f32) -> Buffer {
        let mut buf = Buffer::new(&mut self.fonts, Metrics::new(size, size * LINE_SPACING));
        buf.set_size(None, None);
        let attrs = Attrs::new().family(Family::Name(FAMILY)).weight(Weight::BOLD);
        buf.set_text(text, &attrs, Shaping::Advanced, None);
        buf.shape_until_scroll(&mut self.fonts, false);
        buf
    }

    fn width(&mut self, text: &str, size: f32) -> f32 {
        self.layout(text, size).layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
    }

    /// Number of characters no installed font could draw (they'd show as boxes).
    pub fn missing_glyphs(&mut self, text: &str) -> usize {
        let buf = self.layout(text, 20.0);
        buf.layout_runs().flat_map(|r| r.glyphs.iter()).filter(|g| g.glyph_id == 0).count()
    }

    /// Split `text` at ", " so each piece fits in `avail` pixels where possible.
    fn wrap(&mut self, text: &str, size: f32, avail: f32) -> Vec<String> {
        if self.width(text, size) <= avail {
            return vec![text.to_owned()];
        }
        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        for piece in text.split(", ") {
            let candidate = if current.is_empty() { piece.to_owned() } else { format!("{current}, {piece}") };
            if current.is_empty() || self.width(&candidate, size) <= avail {
                current = candidate;
            } else {
                out.push(format!("{current},"));
                current = piece.to_owned();
            }
        }
        out.push(current);
        out
    }

    /// Burn `lines` into the bottom-left corner of `img`, first line on top.
    /// Works on RGB and RGBA 8-bit images; alpha is raised under the text so it stays visible.
    pub fn stamp<P: Pixel<Subpixel = u8>>(&mut self, img: &mut ImageBuffer<P, Vec<u8>>, lines: &[Line]) {
        if lines.is_empty() {
            return;
        }
        let (w, h) = img.dimensions();
        let short = w.min(h) as f32;
        let margin = (short * 0.035).round().max(4.0);
        let avail = (w as f32 - 2.0 * margin).max(1.0);

        // Base text height ~4% of the short side. Wrap long lines at commas, then shrink if
        // something still doesn't fit.
        let mut base = (short * 0.04).max(12.0);
        let mut wrapped: Vec<(String, f32)> = Vec::new();
        for line in lines {
            for piece in self.wrap(line.text, base * line.rel_size, avail) {
                wrapped.push((piece, line.rel_size));
            }
        }
        let widest = wrapped.iter().map(|(t, rel)| self.width(t, base * rel)).fold(0.0, f32::max);
        if widest > avail {
            base *= avail / widest;
        }
        // Never let the caption cover more than 40% of the photo's height.
        let block_h: f32 = wrapped.iter().map(|(_, rel)| base * rel * LINE_SPACING).sum();
        let max_h = h as f32 * 0.4;
        if block_h > max_h {
            base *= max_h / block_h;
        }

        let outline = (base * 0.09).max(1.0);
        let blur_sigma = (base * 0.15).max(1.0);
        let shadow_offset = (base * 0.06).round() as i64;
        let pad = (outline + 3.0 * blur_sigma).ceil() as i64 + shadow_offset;

        // Lay out lines bottom-up from the bottom margin.
        let mut placed: Vec<(Buffer, f32, f32)> = Vec::with_capacity(wrapped.len());
        let mut bottom = h as f32 - margin;
        for (text, rel) in wrapped.iter().rev() {
            let size = base * rel;
            let buf = self.layout(text, size);
            let line_w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            let top = bottom - size * LINE_SPACING;
            placed.push((buf, top, line_w));
            bottom = top;
        }

        // Work in a local region around the text only; full-size masks on 50 MP photos are slow.
        let text_w = placed.iter().map(|p| p.2).fold(0.0, f32::max).ceil() as i64;
        let top = placed.iter().map(|p| p.1).fold(f32::MAX, f32::min);
        let rx0 = (margin as i64 - pad).max(0);
        let ry0 = (top as i64 - pad).max(0);
        let rx1 = (margin as i64 + text_w + pad).min(w as i64);
        let ry1 = h as i64;
        if rx1 <= rx0 || ry1 <= ry0 {
            return;
        }
        let (rw, rh) = ((rx1 - rx0) as u32, (ry1 - ry0) as u32);

        let mut fill = GrayImage::new(rw, rh);
        for (buf, line_top, _) in &mut placed {
            let ox = margin as i64 - rx0;
            let oy = *line_top as i64 - ry0;
            buf.draw(&mut self.fonts, &mut self.cache, Color::rgb(255, 255, 255), |x, y, gw, gh, color| {
                let a = color.a();
                if a == 0 {
                    return;
                }
                for dy in 0..gh as i64 {
                    for dx in 0..gw as i64 {
                        let (px, py) = (ox + x as i64 + dx, oy + y as i64 + dy);
                        if px >= 0 && py >= 0 && (px as u32) < rw && (py as u32) < rh {
                            let p = fill.get_pixel_mut(px as u32, py as u32);
                            p[0] = p[0].max(a);
                        }
                    }
                }
            });
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
                let channels = px.channels_mut();
                let (color, alpha) = channels.split_at_mut(channels.len().min(3));
                for c in color {
                    let mut v = *c as f32;
                    v -= v * shadow_a;
                    v += (16.0 - v) * outline_a;
                    v += (255.0 - v) * fill_a;
                    *c = v.round().clamp(0.0, 255.0) as u8;
                }
                if let Some(a) = alpha.first_mut() {
                    let cover = shadow_a.max(outline_a).max(fill_a);
                    *a = (*a as f32 + (255.0 - *a as f32) * cover).round() as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

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
        let mut r = Renderer::new();
        for (w, h) in [(800, 600), (600, 800), (64, 48)] {
            let before = RgbImage::from_pixel(w, h, Rgb([120, 160, 200]));
            let mut after = before.clone();
            let lines = [
                Line { text: "Eiffel Tower, Paris", rel_size: 1.0 },
                Line { text: "Wed 12 Jun 2024, 14:32 CEST", rel_size: 0.8 },
            ];
            r.stamp(&mut after, &lines);
            let (x0, y0, x1, _) = changed_bbox(&before, &after).expect("something drawn");
            assert!(x0 < w / 8, "{w}x{h}: left edge {x0}");
            // Caption (plus its shadow) stays in the bottom part of the frame.
            assert!(y0 > h * 2 / 5, "{w}x{h}: top edge {y0}");
            assert!(x1 < w, "{w}x{h}");
        }
    }

    #[test]
    fn legible_on_white_and_black() {
        // A near-white and a near-black pixel must both appear whatever the background:
        // that's the fill/outline contrast pair.
        let mut r = Renderer::new();
        for bg in [0u8, 255] {
            let mut img = RgbImage::from_pixel(400, 300, Rgb([bg, bg, bg]));
            r.stamp(&mut img, &[Line { text: "Hello", rel_size: 1.0 }]);
            assert!(img.pixels().any(|p| p[0] > 240), "bg {bg}: no light text");
            assert!(img.pixels().any(|p| p[0] < 40), "bg {bg}: no dark outline");
        }
    }

    #[test]
    fn long_text_wraps_at_commas_and_fits() {
        let mut r = Renderer::new();
        let before = RgbImage::from_pixel(400, 400, Rgb([90, 90, 90]));
        let mut after = before.clone();
        let long = "Tokyo Metropolitan Government Building (東京都庁舎), Shinjuku (新宿区), Tokyo (東京都)";
        let wrapped = r.wrap(long, 16.0, 380.0);
        assert!(wrapped.len() > 1, "{wrapped:?}");
        assert!(wrapped[0].ends_with(','));
        r.stamp(&mut after, &[Line { text: long, rel_size: 1.0 }]);
        let (_, _, x1, _) = changed_bbox(&before, &after).unwrap();
        assert!(x1 < 400 - 5, "text ran off the edge: {x1}");
    }

    #[test]
    fn non_latin_scripts_have_glyphs() {
        // Needs Noto fonts (installed in the Docker images).
        let mut r = Renderer::new();
        for text in ["Eiffel Tower (Tour Eiffel)", "東京タワー", "Москва", "القاهرة", "กรุงเทพมหานคร"]
        {
            assert_eq!(r.missing_glyphs(text), 0, "{text}");
        }
    }

    #[test]
    fn rgba_alpha_raised_under_text() {
        let mut r = Renderer::new();
        let mut img = RgbaImage::from_pixel(300, 200, Rgba([0, 0, 0, 0]));
        r.stamp(&mut img, &[Line { text: "Hi", rel_size: 1.0 }]);
        assert!(img.pixels().any(|p| p[3] == 255 && p[0] > 240));
    }
}
