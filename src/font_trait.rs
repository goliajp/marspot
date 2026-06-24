//! Phase 10 — `Rasteriser` trait + macOS impls + a no-Metal `MockRasteriser`
//! for headless tests.
//!
//! Spec — docs/font-rendering-design.md §14 + §16 Phase 10.  The
//! atlas wants to be platform-agnostic at the natural-bbox path:
//! Linux/Windows would substitute their own glyph rasteriser (rustybuzz +
//! ab_glyph or DirectWrite) while reusing the marspot shelf packer,
//! eviction policy, and atlas storage.  Phase 10 plumbs only the
//! `get_or_rasterize_natural` path through a trait; the PTY-side
//! `get_or_rasterize` (n_cells + cell-fit safety net) stays as
//! direct CoreText calls until cross-platform PTY rendering is on the
//! table.
//!
//! The trait deliberately does NOT couple to Metal — `RasterOutput`
//! is plain `Vec<u8>` + dims + bearing, so a test can plug a
//! `MockRasteriser` into an atlas backed by a `MockTexture` (or a
//! real Metal texture when one's available) without spinning up a
//! GPU device.

use core_graphics::font::CGGlyph;
use core_text::font::CTFont;

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
    fn rasterise(
        &self,
        font: &CTFont,
        glyph: CGGlyph,
        subpx_x: u8,
    ) -> Option<RasterOutput>;
}

/// macOS-native rasteriser.  Mono variant emits alpha-8; colour
/// variant emits BGRA8 (Apple Color Emoji etc.).  Both delegate to
/// the existing free functions in `glyph_atlas` (Phase 1.1 natural
/// path with Phase 4 sub-pixel offset).  Two flat impls instead of
/// one with a flag because the byte format differs and the atlas
/// already routes on `bpp` — keeping the trait impls symmetrical
/// makes downstream type errors loud.
pub struct CoreTextMonoRasteriser;
pub struct CoreTextColorRasteriser;

impl Rasteriser for CoreTextMonoRasteriser {
    fn rasterise(
        &self,
        font: &CTFont,
        glyph: CGGlyph,
        subpx_x: u8,
    ) -> Option<RasterOutput> {
        crate::glyph_atlas::raster_natural_mono(font, glyph, subpx_x)
    }
}

impl Rasteriser for CoreTextColorRasteriser {
    fn rasterise(
        &self,
        font: &CTFont,
        glyph: CGGlyph,
        subpx_x: u8,
    ) -> Option<RasterOutput> {
        crate::glyph_atlas::raster_natural_color(font, glyph, subpx_x)
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
    fn rasterise(
        &self,
        _font: &CTFont,
        _glyph: CGGlyph,
        _subpx_x: u8,
    ) -> Option<RasterOutput> {
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let r = CoreTextMonoRasteriser
            .rasterise(&font, g, 0)
            .expect("raster A");
        assert!(r.px_w > 0 && r.px_h > 0);
        assert_eq!(r.bytes.len(), (r.px_w * r.px_h) as usize, "mono is R8");
    }

    /// MockRasteriser produces the expected fixed-size buffer with
    /// the right byte count for both mono and colour formats.
    #[test]
    fn mock_rasteriser_emits_canned_bytes() {
        let Ok(font) = new_from_name("Menlo", 13.0) else {
            return;
        };
        let mock = MockRasteriser::mono(4, 8);
        let r = mock.rasterise(&font, 0, 0).expect("mock mono");
        assert_eq!(r.bytes.len(), 4 * 8, "mono = 1 bpp");

        let mock = MockRasteriser::color(4, 8);
        let r = mock.rasterise(&font, 0, 0).expect("mock color");
        assert_eq!(r.bytes.len(), 4 * 8 * 4, "color = 4 bpp");
    }
}
