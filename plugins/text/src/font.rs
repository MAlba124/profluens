//! A self-contained A8 bitmap font for CPU subtitle rasterisation. The atlas is the scope UI's
//! JetBrains Mono (SIL OFL 1.1, licence in `FONT_LICENSE.txt`).

use crate::font_data::{self, GlyphMetrics, SizeMetrics};

/// Glyph 0 is the replacement box, 1..=95 ASCII 0x20..=0x7E, as the scope bake.
const REPLACEMENT: usize = 0;

/// An owned A8 coverage bitmap.
pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub cov: Vec<u8>,
}

impl Bitmap {
    // Vec::new() allocates nothing (zero capacity).
    #[allow(clippy::disallowed_methods)]
    fn empty() -> Self {
        Bitmap { w: 0, h: 0, cov: Vec::new() }
    }

    /// Coverage at `(x, y)`, or 0 out of bounds.
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        if x < self.w && y < self.h { self.cov[y * self.w + x] } else { 0 }
    }
}

/// The baked [`SizeMetrics`] whose pixel size is nearest `px`.
pub fn size_nearest(px: f32) -> &'static SizeMetrics {
    let mut best = &font_data::SIZES[0];
    for s in font_data::SIZES.iter() {
        if (s.px - px).abs() < (best.px - px).abs() {
            best = s;
        }
    }
    best
}

/// The line-box height (px) at a baked size.
pub fn line_height(s: &SizeMetrics) -> usize {
    s.line_h.round() as usize
}

/// The pixel width one line of `text` occupies at size `s`, monospace.
pub fn line_width(s: &SizeMetrics, text: &str) -> usize {
    (text.chars().count() as f32 * s.advance).round() as usize
}

/// Rasterise one line of UTF-8 `text` to an A8 coverage [`Bitmap`] at baked size `s`.
pub fn rasterize_line(s: &SizeMetrics, text: &str) -> Bitmap {
    let w = line_width(s, text);
    let h = line_height(s);
    if w == 0 || h == 0 {
        return Bitmap::empty();
    }
    let mut cov = vec![0u8; w * h];
    let advance = s.advance;
    let mut pen_x = 0.0f32;
    let (aw, ah) = (font_data::ATLAS_W, font_data::ATLAS_H);
    let atlas = font_data::ATLAS_A8;
    for ch in text.chars() {
        let g = &s.glyphs[glyph_index(ch)];
        if g.w > 0 && g.h > 0 {
            blit_glyph(&mut cov, w, h, pen_x, g, atlas, aw, ah);
        }
        pen_x += advance;
    }
    Bitmap { w, h, cov }
}

#[allow(clippy::too_many_arguments)]
fn blit_glyph(
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    pen_x: f32,
    g: &GlyphMetrics,
    atlas: &[u8],
    aw: usize,
    ah: usize,
) {
    let dst_x0 = (pen_x + g.off_x as f32).round() as i32;
    let dst_y0 = g.off_y as i32;
    for gy in 0..g.h as usize {
        let sy = g.y as usize + gy;
        if sy >= ah {
            break;
        }
        let dy = dst_y0 + gy as i32;
        if dy < 0 || dy as usize >= dh {
            continue;
        }
        for gx in 0..g.w as usize {
            let sx = g.x as usize + gx;
            if sx >= aw {
                break;
            }
            let dx = dst_x0 + gx as i32;
            if dx < 0 || dx as usize >= dw {
                continue;
            }
            let a = atlas[sy * aw + sx];
            if a == 0 {
                continue;
            }
            // Max-composite, a safe idempotent merge at the seams.
            let idx = dy as usize * dw + dx as usize;
            if a > dst[idx] {
                dst[idx] = a;
            }
        }
    }
}

fn glyph_index(ch: char) -> usize {
    let c = ch as u32;
    if (0x20..=0x7E).contains(&c) { (c - 0x20 + 1) as usize } else { REPLACEMENT }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_size_snaps_to_a_baked_size() {
        let s = size_nearest(14.0);
        assert!(font_data::SIZES.iter().any(|b| b.px == s.px));
        let small = size_nearest(1.0);
        let big = size_nearest(1000.0);
        assert!(small.px <= big.px);
    }

    #[test]
    fn a_space_inks_nothing_but_advances() {
        let s = size_nearest(14.0);
        let bm = rasterize_line(s, "   ");
        assert!(bm.w > 0, "three spaces still occupy width (advance)");
        assert!(bm.cov.iter().all(|&c| c == 0), "spaces leave zero coverage");
    }

    #[test]
    fn glyphs_produce_ink() {
        let s = size_nearest(14.0);
        let bm = rasterize_line(s, "Hi");
        let inked = bm.cov.iter().filter(|&&c| c > 0).count();
        assert!(inked > 0, "letters leave coverage");
        assert_eq!(bm.h, line_height(s));
        assert_eq!(bm.w, line_width(s, "Hi"));
    }

    #[test]
    fn empty_text_is_an_empty_bitmap() {
        let s = size_nearest(14.0);
        let bm = rasterize_line(s, "");
        assert_eq!((bm.w, bm.h), (0, 0));
        assert!(bm.cov.is_empty());
    }

    #[test]
    fn ink_stays_within_the_line_box() {
        let s = size_nearest(14.0);
        let bm = rasterize_line(s, "Ag|_^");
        assert_eq!(bm.cov.len(), bm.w * bm.h);
    }
}
