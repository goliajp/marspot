//! The seam between the glyph atlas and whoever can turn a font id
//! and a glyph index into pixels.
//!
//! **No platform type appears in `Rasteriser` or `Shaper`.** That is
//! the whole point of the file and `tests/text_layer_is_portable.rs`
//! holds it down: the traits used to take `&CTFont` and `CGGlyph`, so
//! a DirectWrite or FreeType implementation could not be written at
//! all -- it would have had to produce a CoreText font object to
//! satisfy the signature. An implementation now receives a `FontId`,
//! which is the atlas's own key, and looks it up in whatever table it
//! keeps. Resolving and falling back between fonts stays on the
//! platform side, because that is the part each platform must write
//! for itself.
//!
//! `BOX_DRAWING_FONT_ID` is the precedent: an id that corresponds to
//! no font at all, drawn by our own code. The ids were never aliases
//! for CoreText objects, and the traits were the only place that said
//! otherwise.
//!
//! `Shaper` is **not** done: it still takes `&CTFont` and an intern
//! callback handing back `CTFont`s that CoreText discovered mid-shape.
//! Closing it means the font table owns the dedup, which would put
//! the per-cell `is_color_font` lookup behind the table's lock -- a
//! separate decision with a measurable cost, so it is a separate
//! change. The portability test pins the gap so it cannot be
//! forgotten or quietly widened.
//!
//! `RasterOutput` is plain `Vec<u8>` + dims + bearing, so a test can
//! plug a `MockRasteriser` into an atlas backed by a `MockTexture`
//! without a GPU device -- and, now that the terminal grid's path goes
//! through the trait too, drive that path as well.

use std::sync::Arc;

use core_text::font::CTFont;

use crate::font_cache::CoreTextFontTable;
use crate::glyph_atlas::{FontId, SlotMetrics};

/// A glyph index within one font.
///
/// `u16` because that is what every shaper on every platform calls a
/// glyph index: CoreText's `CGGlyph`, DirectWrite's `UINT16`,
/// HarfBuzz's `codepoint_t` in practice. The name no longer points at
/// one of them.
pub type GlyphId = u16;

/// What the atlas needs to record a glyph slot — the pixel bytes plus
/// the geometric metadata the renderer's `quad()` formula expects.
///
/// `bytes` is whatever the upload path can `memcpy` into the atlas
/// texture: alpha-8 for mono atlases, BGRA8 for the colour atlas.
/// The trait impl is responsible for matching `bytes.len()` to the
/// downstream texture's row stride (`px_w * bpp`).
pub struct RasterOutput {
    pub bytes: Vec<u8>,
    pub px_w: u32,
    pub px_h: u32,
    pub bearing_x: i16,
    pub bearing_y: i16,
}

/// Natural-bbox glyph rasteriser.  See module doc for scope.
///
/// `subpx_x` is the Phase 4 sub-pixel x-bucket (0..4 — 0.25-px
/// precision).  Implementations should offset the pen by
/// `subpx_x × 0.25 px` so a single glyph at 4 sub-pixel positions
/// caches as 4 distinct atlas entries.
pub trait Rasteriser: Send + Sync + 'static {
    /// Natural-bbox raster: the glyph at whatever size the font says,
    /// used by chrome where there is no cell to fit.
    fn rasterise(&self, font: FontId, glyph: GlyphId, subpx_x: u8) -> Option<RasterOutput>;

    /// Cell-fitted raster for the terminal grid: the glyph is drawn
    /// into an `n_cells`-wide slot of `metrics`, shrunk if it would
    /// overflow.
    ///
    /// A separate method rather than an option on the first because
    /// the two have different contracts -- this one promises the
    /// output fits the slot, which is what lets every glyph in a row
    /// share one baseline.
    fn rasterise_in_cell(
        &self,
        font: FontId,
        glyph: GlyphId,
        metrics: SlotMetrics,
        n_cells: u16,
    ) -> Option<RasterOutput>;
}

/// macOS-native rasteriser.  Mono variant emits alpha-8; colour
/// variant emits BGRA8 (Apple Color Emoji etc.).  Both delegate to
/// the existing free functions in `glyph_atlas` (Phase 1.1 natural
/// path with Phase 4 sub-pixel offset).  Two flat impls instead of
/// one with a flag because the byte format differs and the atlas
/// already routes on `bpp` — keeping the trait impls symmetrical
/// makes downstream type errors loud.
/// Each holds the font table it resolves ids against.  That table is
/// the platform's -- it is what the trait exists to keep out of the
/// atlas.
pub struct CoreTextMonoRasteriser {
    pub fonts: Arc<CoreTextFontTable>,
}
pub struct CoreTextColorRasteriser {
    pub fonts: Arc<CoreTextFontTable>,
}

impl Rasteriser for CoreTextMonoRasteriser {
    fn rasterise(&self, font: FontId, glyph: GlyphId, subpx_x: u8) -> Option<RasterOutput> {
        let f = self.fonts.get(font as usize)?;
        crate::glyph_atlas::raster_natural_mono(&f, glyph, subpx_x)
    }

    fn rasterise_in_cell(
        &self,
        font: FontId,
        glyph: GlyphId,
        metrics: SlotMetrics,
        n_cells: u16,
    ) -> Option<RasterOutput> {
        let f = self.fonts.get(font as usize)?;
        crate::glyph_atlas::raster_in_cell_mono(&f, glyph, metrics, n_cells)
    }
}

impl Rasteriser for CoreTextColorRasteriser {
    fn rasterise(&self, font: FontId, glyph: GlyphId, subpx_x: u8) -> Option<RasterOutput> {
        let f = self.fonts.get(font as usize)?;
        crate::glyph_atlas::raster_natural_color(&f, glyph, subpx_x)
    }

    fn rasterise_in_cell(
        &self,
        font: FontId,
        glyph: GlyphId,
        metrics: SlotMetrics,
        n_cells: u16,
    ) -> Option<RasterOutput> {
        let f = self.fonts.get(font as usize)?;
        crate::glyph_atlas::raster_in_cell_color(&f, glyph, metrics, n_cells)
    }
}

/// Headless test rasteriser — returns a tiny canned bitmap of the
/// requested bytes-per-pixel.  Bypasses CoreText entirely so unit
/// tests can exercise the atlas's shelf packer + LRU eviction
/// without standing up a Metal device.  `bpp` selects mono (R8 = 1)
/// or colour (BGRA8 = 4).
pub struct MockRasteriser {
    pub bpp: u32,
    pub px_w: u32,
    pub px_h: u32,
}

impl MockRasteriser {
    pub fn mono(px_w: u32, px_h: u32) -> Self {
        Self { bpp: 1, px_w, px_h }
    }
    pub fn color(px_w: u32, px_h: u32) -> Self {
        Self { bpp: 4, px_w, px_h }
    }
}

impl Rasteriser for MockRasteriser {
    fn rasterise(&self, _font: FontId, _glyph: GlyphId, _subpx_x: u8) -> Option<RasterOutput> {
        self.canned()
    }

    /// The slot the grid asked for, filled -- so a headless test can
    /// drive the terminal path and compare bytes.
    fn rasterise_in_cell(
        &self,
        _font: FontId,
        _glyph: GlyphId,
        metrics: SlotMetrics,
        n_cells: u16,
    ) -> Option<RasterOutput> {
        let px_w = metrics.cell_w * n_cells.max(1) as u32;
        let px_h = metrics.cell_h;
        Some(RasterOutput {
            bytes: vec![0xFFu8; (px_w * px_h * self.bpp) as usize],
            px_w,
            px_h,
            bearing_x: 0,
            bearing_y: metrics.baseline_from_top as i16,
        })
    }
}

impl MockRasteriser {
    fn canned(&self) -> Option<RasterOutput> {
        let n = (self.px_w * self.px_h * self.bpp) as usize;
        Some(RasterOutput {
            bytes: vec![0xFFu8; n],
            px_w: self.px_w,
            px_h: self.px_h,
            // Match the Phase 1.1 lsb-pre-cancelled bearings the
            // real rasteriser reports, so the renderer's `quad()`
            // formula yields the same offsets regardless of whether
            // this is mock data or a real glyph.
            bearing_x: -1,
            bearing_y: self.px_h as i16 - 1,
        })
    }
}

/// Phase 10b — line shaper.  Same scope as `Rasteriser`: macOS impl
/// wraps `font_shape::shape_line`; tests can plug `MockShaper` to
/// drive the `FontCache.shape_cache` without spinning up CoreText.
///
/// The trait method takes a `&mut dyn FnMut(CTFont) -> u32` for the
/// fallback-font intern callback, so callers (FontCache) can hand
/// the closure their `FontRegistry::intern` without committing to a
/// generic type parameter on the trait.  `&dyn` keeps the trait
/// object-safe.
pub trait Shaper: Send + Sync + 'static {
    fn shape(
        &self,
        text: &str,
        base_font: &CTFont,
        opts: crate::font_shape::ShapeOptions,
        intern: &mut dyn FnMut(CTFont) -> u32,
    ) -> Vec<crate::font_shape::ShapedGlyph>;
}

/// macOS-native shaper: thin wrapper around `font_shape::shape_line`.
pub struct CoreTextShaper;

impl Shaper for CoreTextShaper {
    fn shape(
        &self,
        text: &str,
        base_font: &CTFont,
        opts: crate::font_shape::ShapeOptions,
        intern: &mut dyn FnMut(CTFont) -> u32,
    ) -> Vec<crate::font_shape::ShapedGlyph> {
        crate::font_shape::shape_line(text, base_font, opts, intern)
    }
}

/// Headless test shaper.  Returns the pre-built `canned` glyphs
/// regardless of input — useful for testing the chrome pipeline's
/// glyph-pushing path without depending on a real font.
pub struct MockShaper {
    pub canned: Vec<crate::font_shape::ShapedGlyph>,
}

impl Shaper for MockShaper {
    fn shape(
        &self,
        _text: &str,
        _base_font: &CTFont,
        _opts: crate::font_shape::ShapeOptions,
        _intern: &mut dyn FnMut(CTFont) -> u32,
    ) -> Vec<crate::font_shape::ShapedGlyph> {
        self.canned.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::font::CGGlyph;
    use core_text::font::new_from_name;

    /// Sanity — the macOS impl returns SOMETHING for a real glyph
    /// without panicking.  Most rasteriser behaviour is exercised
    /// indirectly through the atlas tests; this test exists so a
    /// future refactor that breaks the trait routing fails noisily
    /// here first.
    #[test]
    fn coretext_mono_returns_raster() {
        let Ok(font) = new_from_name("Menlo", 13.0) else {
            eprintln!("skipping: no Menlo");
            return;
        };
        let mut g: CGGlyph = 0;
        let cu: u16 = b'A' as u16;
        unsafe {
            font.get_glyphs_for_characters(&cu, &mut g, 1);
        }
        let r = CoreTextMonoRasteriser { fonts: CoreTextFontTable::single(font) }
            .rasterise(0, g, 0)
            .expect("raster A");
        assert!(r.px_w > 0 && r.px_h > 0);
        assert_eq!(r.bytes.len(), (r.px_w * r.px_h) as usize, "mono is R8");
    }

    /// MockShaper returns its canned glyphs regardless of input.
    /// Verifies the trait dispatch shape so FontCache can plug a
    /// custom shaper for headless tests.
    #[test]
    fn mock_shaper_emits_canned_glyphs() {
        let Ok(font) = new_from_name("Menlo", 13.0) else {
            return;
        };
        let canned = vec![
            crate::font_shape::ShapedGlyph {
                font_id: 99,
                glyph_id: 7,
                pen_x_px: 0,
                subpx_x: 0,
            },
            crate::font_shape::ShapedGlyph {
                font_id: 99,
                glyph_id: 8,
                pen_x_px: 12,
                subpx_x: 2,
            },
        ];
        let shaper = MockShaper { canned: canned.clone() };
        let mut intern_calls = 0;
        let mut intern = |_f: CTFont| -> u32 {
            intern_calls += 1;
            0
        };
        let out = shaper.shape(
            "ignored",
            &font,
            crate::font_shape::ShapeOptions::full(),
            &mut intern,
        );
        assert_eq!(out.len(), canned.len());
        assert_eq!(out[0].glyph_id, canned[0].glyph_id);
        assert_eq!(out[1].pen_x_px, canned[1].pen_x_px);
        assert_eq!(intern_calls, 0, "mock should not call intern");
    }

    /// CoreTextShaper just delegates — calling `shape_line` directly
    /// vs through the trait must produce identical glyph sequences
    /// for the same input.
    #[test]
    fn coretext_shaper_matches_shape_line() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let opts = crate::font_shape::ShapeOptions::full();
        let direct = crate::font_shape::shape_line("Hi!", &font, opts, |_| 0);
        let mut via_trait_intern = |_f: CTFont| -> u32 { 0 };
        let via_trait = CoreTextShaper.shape("Hi!", &font, opts, &mut via_trait_intern);
        assert_eq!(direct.len(), via_trait.len(), "trait must match free fn");
        for i in 0..direct.len() {
            assert_eq!(direct[i].glyph_id, via_trait[i].glyph_id);
            assert_eq!(direct[i].pen_x_px, via_trait[i].pen_x_px);
        }
    }

    /// MockRasteriser produces the expected fixed-size buffer with
    /// the right byte count for both mono and colour formats.
    #[test]
    fn mock_rasteriser_emits_canned_bytes() {
        let mock = MockRasteriser::mono(4, 8);
        let r = mock.rasterise(0, 0, 0).expect("mock mono");
        assert_eq!(r.bytes.len(), 4 * 8, "mono = 1 bpp");

        let mock = MockRasteriser::color(4, 8);
        let r = mock.rasterise(0, 0, 0).expect("mock color");
        assert_eq!(r.bytes.len(), 4 * 8 * 4, "color = 4 bpp");
    }

    /// The cell-fitted method fills exactly the slot the grid asked
    /// for -- that is the contract that lets a headless test drive the
    /// terminal path and compare bytes against the real rasteriser's
    /// output shape.
    #[test]
    fn mock_fills_the_slot_the_grid_asked_for() {
        let m = SlotMetrics { cell_w: 7, cell_h: 15, baseline_from_top: 11 };
        let r = MockRasteriser::mono(1, 1)
            .rasterise_in_cell(0, 0, m, 2)
            .expect("mock in-cell");
        assert_eq!((r.px_w, r.px_h), (14, 15), "two cells wide, one tall");
        assert_eq!(r.bytes.len(), 14 * 15);
        assert_eq!(r.bearing_y, 11, "baseline comes from the metrics");
    }
}
