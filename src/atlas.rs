//! Glyph atlas: CPU-side rasterization of font glyphs into a single
//! grayscale texture, packed via a simple shelf packer.
//!
//! Phase 1.2.1 only handles the data side — building the atlas image and
//! the char→UV map.  GPU upload (creating an `MTLTexture`) and rendering
//! land in 1.2.2/1.2.3.
//!
//! Why CPU rasterization for a GPU renderer:
//! - Terminal text uses small fixed sizes (12–14pt) where CoreText's
//!   hinting + LCD subpixel filtering is dramatically more legible than
//!   any naive GPU sampling.
//! - The cost is paid **once per glyph** and cached.  Render hot path is
//!   pure GPU sampling against this atlas.
//! - Same path Apple's Terminal.app, iTerm2, Ghostty, kitty, alacritty
//!   all use.  Visual output matches the rest of macOS.

use core_graphics::base::{kCGImageAlphaPremultipliedFirst, CGFloat};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::font::CGGlyph;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_text::font::{new_from_name, CTFont};
use core_text::font_descriptor::kCTFontOrientationDefault;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphInfo {
    /// Top-left position within the atlas, in pixels.
    pub atlas_x: u32,
    pub atlas_y: u32,
    /// Glyph bitmap dimensions, in pixels.
    pub width: u32,
    pub height: u32,
    /// Where to draw the glyph relative to the cell's pen origin (origin
    /// = baseline at the left edge).  In CoreGraphics's coordinate space:
    /// y is positive going up, so most glyphs have `bearing_y >= 0` and
    /// descenders push it negative.
    pub bearing_x: f32,
    pub bearing_y: f32,
}

pub struct GlyphAtlas {
    /// Single-channel grayscale, row-major top-down: `pixels[y * width + x]`
    /// is the alpha intensity (0 = empty, 255 = full glyph coverage).
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    glyphs: HashMap<char, GlyphInfo>,
    /// Shelf packer: glyphs flow left-to-right.  When a glyph won't fit
    /// on the current shelf, advance to a new one of height equal to the
    /// tallest glyph on the previous shelf.
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    font: CTFont,
}

impl GlyphAtlas {
    /// Build an atlas for `font_name` at `point_size`, backing a square
    /// `atlas_size` × `atlas_size` grayscale image.  Falls back to Menlo
    /// if the named font isn't available.
    pub fn new(font_name: &str, point_size: f32, atlas_size: u32) -> Self {
        let font = new_from_name(font_name, point_size as f64)
            .or_else(|_| new_from_name("Menlo", point_size as f64))
            .expect("Menlo fallback should exist on every macOS install");
        Self {
            pixels: vec![0u8; (atlas_size * atlas_size) as usize],
            width: atlas_size,
            height: atlas_size,
            glyphs: HashMap::new(),
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            font,
        }
    }

    pub fn pixels(&self) -> &[u8] { &self.pixels }
    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }
    pub fn glyph_count(&self) -> usize { self.glyphs.len() }

    /// Pixel width of one monospace cell.  We sample 'M' as the
    /// representative advance — for fixed-pitch fonts every glyph has the
    /// same advance, so this is exact for Menlo/SF Mono/etc.
    pub fn cell_width(&self) -> f32 {
        let mut glyph: CGGlyph = 0;
        let ch: u16 = b'M' as u16;
        let ok = unsafe { self.font.get_glyphs_for_characters(&ch, &mut glyph, 1) };
        if !ok || glyph == 0 {
            return 0.0;
        }
        let mut size = core_graphics::geometry::CGSize::new(0.0, 0.0);
        unsafe {
            self.font.get_advances_for_glyphs(
                kCTFontOrientationDefault,
                &glyph,
                &mut size,
                1,
            );
        }
        size.width as f32
    }

    /// Pixel height per row: ascent + descent + leading.  Defines vertical
    /// spacing between baselines.
    pub fn cell_height(&self) -> f32 {
        (self.font.ascent() + self.font.descent() + self.font.leading()) as f32
    }

    /// Distance from the top of a cell down to the baseline.  Used to
    /// position glyphs vertically within a cell.
    pub fn ascent(&self) -> f32 {
        self.font.ascent() as f32
    }

    /// Look up a glyph that's already in the atlas without rasterizing.
    pub fn get(&self, ch: char) -> Option<&GlyphInfo> {
        self.glyphs.get(&ch)
    }

    /// Look up `ch` in the atlas, rasterizing on cache miss.  Returns `None`
    /// if the atlas is full and the new glyph won't fit, or if the font
    /// has no glyph for this character.
    pub fn ensure(&mut self, ch: char) -> Option<&GlyphInfo> {
        if self.glyphs.contains_key(&ch) {
            return self.glyphs.get(&ch);
        }
        let info = self.rasterize(ch)?;
        self.glyphs.insert(ch, info);
        self.glyphs.get(&ch)
    }

    fn rasterize(&mut self, ch: char) -> Option<GlyphInfo> {
        // BMP-only for now.  Surrogate pairs / non-BMP codepoints land in
        // a later refinement (the glyphs_for_characters API takes UTF-16
        // input where non-BMP requires two units).
        let cp = ch as u32;
        if cp > 0xFFFF {
            return None;
        }
        let utf16 = cp as u16;

        let mut glyph: CGGlyph = 0;
        // SAFETY: passing a single u16 in and a single CGGlyph out, count = 1.
        let ok = unsafe {
            self.font.get_glyphs_for_characters(&utf16, &mut glyph, 1)
        };
        if !ok || glyph == 0 {
            return None;
        }

        let bbox = self
            .font
            .get_bounding_rects_for_glyphs(kCTFontOrientationDefault, &[glyph]);

        let glyph_w = bbox.size.width.ceil() as u32;
        let glyph_h = bbox.size.height.ceil() as u32;

        // Empty glyphs (e.g. the space character) — record a zero-extent
        // entry so callers can read advance/bearing without re-rasterizing,
        // and don't consume any atlas space.
        if glyph_w == 0 || glyph_h == 0 {
            return Some(GlyphInfo {
                atlas_x: 0,
                atlas_y: 0,
                width: 0,
                height: 0,
                bearing_x: bbox.origin.x as f32,
                bearing_y: bbox.origin.y as f32,
            });
        }

        // Rasterize via an RGBA premultiplied context (matching the
        // pipeline alacritty/crossfont use): black opaque background,
        // white glyph fill, all CT smoothing/subpixel hints ON so we get
        // the visual weight Apple's Terminal.app and iTerm2 produce.
        // After rasterization the glyph's coverage is encoded in any one
        // of R/G/B (they're equal for grayscale) — we extract R into the
        // atlas's single-channel buffer.
        let space = CGColorSpace::create_device_rgb();
        let row_bytes = glyph_w as usize * 4;
        let mut ctx = CGContext::create_bitmap_context(
            None,
            glyph_w as usize,
            glyph_h as usize,
            8,
            row_bytes,
            &space,
            kCGImageAlphaPremultipliedFirst,
        );
        // Fill the bitmap with opaque black.  CT's smoothing pass works
        // against an opaque background — that's the "stem-darkening"
        // behavior that gives glyphs their visual weight.
        ctx.set_rgb_fill_color(0.0, 0.0, 0.0, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(glyph_w as f64, glyph_h as f64),
        ));
        // Enable every smoothing knob (matches Apple's default + alacritty).
        ctx.set_allows_antialiasing(true);
        ctx.set_should_antialias(true);
        ctx.set_allows_font_smoothing(true);
        ctx.set_should_smooth_fonts(true);
        ctx.set_allows_font_subpixel_positioning(true);
        ctx.set_should_subpixel_position_fonts(true);
        ctx.set_allows_font_subpixel_quantization(true);
        ctx.set_should_subpixel_quantize_fonts(true);
        // White glyph on the black backdrop — the resulting RGB channels
        // ARE the per-channel coverage from CT's smoothing pass.
        ctx.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
        self.font.draw_glyphs(
            &[glyph],
            &[CGPoint::new(-bbox.origin.x as CGFloat, -bbox.origin.y as CGFloat)],
            ctx.clone(),
        );

        let (atlas_x, atlas_y) = self.allocate_shelf_slot(glyph_w, glyph_h)?;

        // Bitmap layout under kCGImageAlphaPremultipliedFirst on macOS
        // little-endian is A,R,G,B per pixel in memory order.  Background
        // is opaque black so A=255 everywhere — we cannot use it as the
        // coverage signal.  R/G/B carry the actual coverage (white text
        // means R=G=B=255 at the glyph center, decaying with antialiasing
        // toward 0 at the edges).  Pick R as canonical (G/B equal it for
        // grayscale rendering).
        let src = ctx.data();
        for row in 0..glyph_h {
            for col in 0..glyph_w as usize {
                let src_idx = (row as usize) * row_bytes + col * 4;
                // [A, R, G, B] in memory order — index 1 is R.
                let coverage = src[src_idx + 1];
                let dst_idx = ((atlas_y + row) as usize) * (self.width as usize)
                    + atlas_x as usize
                    + col;
                self.pixels[dst_idx] = coverage;
            }
        }

        Some(GlyphInfo {
            atlas_x,
            atlas_y,
            width: glyph_w,
            height: glyph_h,
            bearing_x: bbox.origin.x as f32,
            bearing_y: bbox.origin.y as f32,
        })
    }

    /// Reserve a `(w × h)` rectangle on the next available shelf, with a
    /// `GLYPH_PAD` buffer of empty pixels around it.  The buffer prevents
    /// the GPU's linear sampler from bleeding adjacent glyphs into a
    /// glyph's edges (the "white halo" artifact).
    fn allocate_shelf_slot(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        const GLYPH_PAD: u32 = 1;
        let total_w = w + GLYPH_PAD * 2;
        let total_h = h + GLYPH_PAD * 2;
        if total_w > self.width {
            return None;
        }
        if self.shelf_x + total_w > self.width {
            self.shelf_y += self.shelf_h;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        if self.shelf_y + total_h > self.height {
            return None;
        }
        // The actual glyph lands GLYPH_PAD pixels in from the slot's corner.
        let placed = (self.shelf_x + GLYPH_PAD, self.shelf_y + GLYPH_PAD);
        self.shelf_x += total_w;
        if total_h > self.shelf_h {
            self.shelf_h = total_h;
        }
        Some(placed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_atlas() -> GlyphAtlas {
        GlyphAtlas::new("Menlo", 13.0, 256)
    }

    #[test]
    fn new_atlas_is_empty_and_zero_pixels() {
        let a = fresh_atlas();
        assert_eq!(a.glyph_count(), 0);
        assert_eq!(a.width(), 256);
        assert_eq!(a.height(), 256);
        assert!(a.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn ensure_rasterizes_ascii_and_returns_info() {
        let mut a = fresh_atlas();
        let info = *a.ensure('A').expect("'A' should rasterize");
        assert!(info.width > 0 && info.height > 0, "'A' bitmap should be non-empty");
        // The glyph must have been written into the atlas at the recorded
        // position — at least one pixel inside the rect must be non-zero.
        let any_lit = (0..info.height).any(|dy| {
            (0..info.width).any(|dx| {
                let idx = ((info.atlas_y + dy) as usize) * 256 + (info.atlas_x + dx) as usize;
                a.pixels()[idx] != 0
            })
        });
        assert!(any_lit, "'A' rasterized bitmap should have some lit pixels");
    }

    #[test]
    fn ensure_caches_subsequent_lookups() {
        let mut a = fresh_atlas();
        let first = *a.ensure('B').expect("first lookup");
        let count_after_first = a.glyph_count();
        let second = *a.ensure('B').expect("cached lookup");
        assert_eq!(first, second, "repeated ensure must return identical entry");
        assert_eq!(a.glyph_count(), count_after_first, "no new entry on cache hit");
    }

    #[test]
    fn shelf_packer_advances_left_to_right_then_wraps() {
        let mut a = fresh_atlas();
        let i1 = *a.ensure('A').unwrap();
        let i2 = *a.ensure('B').unwrap();
        // Two glyphs on the same shelf — second must be to the right of
        // first and on the same y row.
        assert_eq!(i1.atlas_y, i2.atlas_y, "first two glyphs share a shelf");
        assert!(i2.atlas_x >= i1.atlas_x + i1.width, "second placed after first");
    }

    #[test]
    fn shelf_packer_wraps_to_new_shelf_when_full() {
        // Use a small atlas that forces wrapping after a few glyphs.
        let mut a = GlyphAtlas::new("Menlo", 13.0, 32);
        let mut last_y = 0u32;
        let mut wrapped = false;
        for ch in "ABCDEFGH".chars() {
            if let Some(info) = a.ensure(ch) {
                if info.atlas_y > last_y {
                    wrapped = true;
                }
                last_y = info.atlas_y;
            }
        }
        assert!(wrapped, "32px-wide atlas should force a shelf wrap within 8 ASCII glyphs");
    }

    #[test]
    fn ensure_returns_none_when_atlas_is_full() {
        // Tiny atlas: one glyph fits at most.
        let mut a = GlyphAtlas::new("Menlo", 13.0, 16);
        let mut placed = 0;
        let mut overflowed = false;
        for ch in 'A'..'z' {
            if a.ensure(ch).is_none() {
                overflowed = true;
                break;
            }
            placed += 1;
        }
        assert!(overflowed, "16x16 atlas should fill before exhausting ASCII range");
        assert!(placed > 0, "at least one glyph should fit");
    }

    #[test]
    fn space_character_caches_zero_extent_entry() {
        // The blank space glyph has no visible bitmap; cache it as a
        // zero-extent record so we don't re-rasterize.
        let mut a = fresh_atlas();
        let info = a.ensure(' ').expect("space should at least cache an entry");
        assert_eq!(info.width, 0);
        assert_eq!(info.height, 0);
        // Re-ensure should hit cache.
        let count = a.glyph_count();
        let _ = a.ensure(' ');
        assert_eq!(a.glyph_count(), count);
    }
}
