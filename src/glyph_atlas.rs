//! Glyph atlas — alpha-only MTLTexture that holds rasterised glyphs
//! the FG-pass shader samples.
//!
//! Used by the Metal renderer (`render_metal.rs`).  Lives in its own
//! module because the responsibilities are clean: rasterise CT glyphs
//! into a packed texture, look them up by `(font_id, glyph_id)`, evict
//! when full.  The renderer doesn't need to know how packing or
//! eviction work.
//!
//! ## Format
//!
//! `MTLPixelFormat::R8Unorm`, single channel = alpha coverage 0..255.
//! Colour comes from a per-cell uniform in the FG shader, so the atlas
//! stays font-colour-agnostic and cells of different colours share
//! glyph slots.
//!
//! ## Packing
//!
//! Shelf packer (the simpler cousin of skyline).  Each shelf has a
//! fixed height; new glyphs go on the first shelf with `height` close
//! enough and `x_used + glyph_w <= atlas_w`, else a new shelf opens.
//! Skyline would pack ~5–10 % tighter but the atlas isn't tight on
//! space — Menlo 13pt at 2× Retina = ~16×32 px per glyph, and 95
//! printable ASCII fit in <100 KiB of atlas.  Shelves are simpler to
//! reason about.
//!
//! ## Eviction (CLAUDE.md "bounded growth")
//!
//! When `place()` can't fit a new glyph, `get_or_rasterize` does an
//! **atomic rebuild**: drops every shelf + clears the cache, then
//! retries the placement on the (now empty) atlas.  Next-frame
//! re-rasterises whichever glyphs are still on screen.  One stutter
//! frame at the boundary, then back to bounded steady-state.
//!
//! The earlier "Phase 2 — return None, render notdef" plan was wrong
//! on two counts: (1) the renderer didn't actually render notdef, it
//! silently `continue`'d the cell, leaving the user-visible glyph
//! invisible; (2) the assumption that "terminal use rarely rotates
//! more than a few hundred unique glyphs" breaks under the realistic
//! 9-session use case — each session feeds the same atlas, every
//! bold/italic variant takes its own slot, and CJK + emoji blow
//! past 1500 glyphs in hours of normal use.
//!
//! Atomic rebuild is bounded (atlas size is fixed forever) and
//! self-healing (visible chars come back next frame).  Texture pixel
//! data isn't cleared — UV coords are tight, so the regions we don't
//! re-upload are simply unsampled.
//!
//! ## Padding
//!
//! Each glyph gets a 1-px transparent border in the atlas, so linear
//! sampling at the edges doesn't bleed in neighbours.  This was one
//! of the bugs that killed the previous Metal+atlas attempt
//! (`render.rs`'s header note "atlas neighbor … issues").

use core_graphics::base::CGFloat;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_graphics::font::CGGlyph;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_text::font::CTFont;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

/// `kCGImageAlphaOnly` — Apple's `CGImageAlphaInfo` value for an
/// alpha-only bitmap context.  Not exposed as a constant by
/// `core-graphics` 0.24, so we declare it directly (it's part of
/// the stable public API of CoreGraphics).
const KCGIMAGE_ALPHA_ONLY: u32 = 7;

// FFI for `CGBitmapContextCreate`.  `core-graphics`'s safe wrapper
// requires a non-null `&CGColorSpace`, but alpha-only contexts must
// pass NULL.  Declared here as a private extern so we can build the
// alpha-only context the atlas needs.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        // *mut CGColorSpace — opaque, can be null for AlphaOnly.
        colorspace: *mut c_void,
        bitmap_info: u32,
    ) -> *mut core_graphics::sys::CGContext;
}

pub type FontId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey {
    pub font_id: FontId,
    pub glyph: CGGlyph,
}

/// What the renderer needs to draw a cached glyph: where it lives
/// in the atlas (`u0/v0/u1/v1`, in atlas pixel coords) and the slot
/// dimensions.  Each slot is **exactly cell-sized** — 1 cell wide
/// for normal glyphs, `n_cells` wide for east-asian-wide / emoji.
/// The glyph is rasterised at an integer baseline position INSIDE
/// the slot, identical for every glyph at the same font/size, so
/// the renderer just draws a cell-sized quad at the cell origin —
/// no per-glyph bearing maths, no fractional dest_y, no risk of one
/// glyph's baseline landing 0.5 px below its neighbour's.
#[derive(Clone, Copy, Debug)]
pub struct AtlasEntry {
    pub u0: u16,
    pub v0: u16,
    pub u1: u16,
    pub v1: u16,
    pub px_w: u16,
    pub px_h: u16,
    /// How many terminal cells wide this slot is (1 for ASCII /
    /// most BMP, 2 for East Asian wide / emoji).
    pub n_cells: u16,
}

/// One row in the shelf packer.
#[derive(Clone, Copy, Debug)]
struct Shelf {
    /// Top of the shelf in atlas pixel space (0 = top).
    y: u32,
    /// Allocated height of the shelf.
    h: u32,
    /// Pixels consumed from the left.
    x_used: u32,
}

pub struct GlyphAtlas {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    width: u32,
    height: u32,
    shelves: Vec<Shelf>,
    cache: HashMap<GlyphKey, AtlasEntry>,
    /// Number of times the atlas filled up and was rebuilt.  Each
    /// rebuild costs one stutter frame to re-rasterise visible glyphs.
    /// In steady-state terminal use this should stay at 0; non-zero
    /// after settling means working set exceeds atlas capacity (bump
    /// the atlas dims).
    pub rebuild_count: u64,
}

/// 1-px padding on every side of every glyph; prevents linear
/// filtering from sampling the neighbour above / left.
const PAD: u32 = 1;

impl GlyphAtlas {
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::R8Unorm,
                width as usize,
                height as usize,
                false,
            )
        };
        unsafe {
            // Managed: CPU writes via replaceRegion, GPU reads.  On
            // Apple Silicon (UMA) Shared would also work and skip
            // the synchronize step, but Managed is portable across
            // Intel + Apple Silicon and the perf delta is irrelevant
            // for an atlas updated on cache miss only.
            descriptor.setStorageMode(MTLStorageMode::Managed);
            descriptor.setUsage(MTLTextureUsage::ShaderRead);
        }
        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        Ok(Self {
            texture,
            width,
            height,
            shelves: Vec::new(),
            cache: HashMap::new(),
            rebuild_count: 0,
        })
    }

    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Look up `key` in the cache, rasterising into the atlas on miss.
    /// Slot dimensions come from the caller's `metrics`; every glyph
    /// for the same font gets a slot of the same size, with the glyph
    /// drawn at an integer baseline position inside.  Renderers draw
    /// cell-sized quads so all glyphs share an exact baseline row —
    /// see the AtlasEntry doc.
    ///
    /// `Some(entry)` on success; `None` only when the glyph itself has
    /// no ink (control char, .notdef-with-degenerate-bbox).  Atlas-full
    /// triggers atomic rebuild and retry, never returns None.
    pub fn get_or_rasterize(
        &mut self,
        key: GlyphKey,
        font: &CTFont,
        metrics: SlotMetrics,
    ) -> Option<AtlasEntry> {
        if let Some(&entry) = self.cache.get(&key) {
            return Some(entry);
        }
        let raster = rasterise_glyph(font, key.glyph, metrics)?;
        let placed = match self.place(raster.px_w, raster.px_h) {
            Some(p) => p,
            None => {
                self.rebuild();
                self.place(raster.px_w, raster.px_h)?
            }
        };
        self.upload(&raster.bytes, raster.px_w, raster.px_h, placed.0, placed.1);
        let entry = AtlasEntry {
            u0: placed.0 as u16,
            v0: placed.1 as u16,
            u1: (placed.0 + raster.px_w) as u16,
            v1: (placed.1 + raster.px_h) as u16,
            px_w: raster.px_w as u16,
            px_h: raster.px_h as u16,
            n_cells: raster.n_cells,
        };
        self.cache.insert(key, entry);
        Some(entry)
    }

    /// Drop every shelf + clear the lookup cache.  Texture pixel data
    /// is left as-is; tight UV coords ensure unsampled regions don't
    /// leak through.  Called from `get_or_rasterize` when the atlas is
    /// full; ~ASCII-set's worth of glyphs re-rasterise on the next
    /// frame.
    fn rebuild(&mut self) {
        self.shelves.clear();
        self.cache.clear();
        self.rebuild_count += 1;
    }

    /// Find a position for a `(w, h)` glyph using the shelf packer.
    /// Returns the top-left pixel coords inside the atlas, or `None`
    /// when there's no room.
    fn place(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let pad_w = w + 2 * PAD;
        let pad_h = h + 2 * PAD;
        if pad_w > self.width {
            // Glyph wider than the atlas — pathological; bail.
            return None;
        }

        // Try to place on an existing shelf whose height fits.  We
        // accept up to 25 % wasted vertical space — past that, open
        // a new shelf so glyphs of similar height cluster together.
        for shelf in &mut self.shelves {
            if shelf.h >= pad_h && (shelf.h as f64 - pad_h as f64) / shelf.h as f64 <= 0.25
                && shelf.x_used + pad_w <= self.width
            {
                let x = shelf.x_used + PAD;
                let y = shelf.y + PAD;
                shelf.x_used += pad_w;
                return Some((x, y));
            }
        }

        // Open a new shelf at the bottom of the atlas.
        let y_top = self.shelves.last().map(|s| s.y + s.h).unwrap_or(0);
        if y_top + pad_h > self.height {
            return None;
        }
        self.shelves.push(Shelf {
            y: y_top,
            h: pad_h,
            x_used: pad_w,
        });
        Some((PAD, y_top + PAD))
    }

    fn upload(&self, bytes: &[u8], w: u32, h: u32, dst_x: u32, dst_y: u32) {
        if bytes.is_empty() {
            return;
        }
        let region = MTLRegion {
            origin: MTLOrigin {
                x: dst_x as usize,
                y: dst_y as usize,
                z: 0,
            },
            size: MTLSize {
                width: w as usize,
                height: h as usize,
                depth: 1,
            },
        };
        unsafe {
            let ptr = NonNull::new(bytes.as_ptr() as *mut c_void).unwrap();
            self.texture
                .replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    ptr,
                    w as usize,
                );
        }
    }

    /// For tests / instrumentation.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

/// Per-render-context slot geometry.  Same for every glyph at a
/// given font/size, supplied by the caller so this module doesn't
/// need to know about FontCache or cell metrics.  Pixel units.
#[derive(Clone, Copy, Debug)]
pub struct SlotMetrics {
    /// Cell width — slot width for non-wide chars.
    pub cell_w: u32,
    /// Cell height — slot height for ALL chars.
    pub cell_h: u32,
    /// Pixels from the slot's TOP edge down to the baseline (y-down).
    /// Glyphs are positioned so their baseline sits exactly on this
    /// integer row, identical for every glyph.
    pub baseline_from_top: u32,
}

/// Output of the CT-rasterise pass.
struct Raster {
    bytes: Vec<u8>,
    px_w: u32,
    px_h: u32,
    n_cells: u16,
}

/// Rasterise one glyph into a CELL-SIZED alpha-only bitmap.  The
/// glyph's baseline is positioned at integer canvas y =
/// `metrics.cell_h - metrics.baseline_from_top` (y-up) — identical
/// for every glyph at the same font/size, so the renderer can place
/// every cell-sized slot at the cell origin and every baseline lines
/// up exactly.
///
/// Wide glyphs (advance > cell_w) get a 2-cell-wide slot.  Glyphs
/// that don't fit even in 2 cells are clipped to fit.
///
/// Returns `None` only if the glyph has no ink (control char,
/// .notdef-with-degenerate-bbox).
fn rasterise_glyph(
    font: &CTFont,
    glyph: CGGlyph,
    metrics: SlotMetrics,
) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }

    // Decide slot width: 2 cells if the glyph's advance is closer to
    // 2× cell_w (CJK / emoji), else 1 cell.  Most fonts already align
    // fullwidth glyphs to a 2-cell advance.
    let advance_w = bbox.size.width;
    let cell_w_f = metrics.cell_w as f64;
    let n_cells: u16 = if advance_w > cell_w_f * 1.5 { 2 } else { 1 };
    let px_w = metrics.cell_w * n_cells as u32;
    let px_h = metrics.cell_h;

    let bytes_per_row = px_w as usize;
    let buf_len = bytes_per_row * px_h as usize;
    let mut bytes: Vec<u8> = vec![0u8; buf_len];

    let ctx = unsafe {
        let raw = CGBitmapContextCreate(
            bytes.as_mut_ptr() as *mut c_void,
            px_w as usize,
            px_h as usize,
            8,
            bytes_per_row,
            std::ptr::null_mut(),
            KCGIMAGE_ALPHA_ONLY,
        );
        if raw.is_null() {
            return None;
        }
        CGContext::from_ptr(raw)
    };

    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_should_smooth_fonts(true);
    ctx.set_allows_font_smoothing(true);
    ctx.set_should_subpixel_position_fonts(true);
    ctx.set_allows_font_subpixel_positioning(true);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    ctx.set_gray_fill_color(1.0, 1.0);

    // CGBitmapContext's default CTM is y-up with origin at lower-left.
    // We want baseline at y-DOWN row `baseline_from_top` from top.
    // In y-up, that's canvas y = `cell_h - baseline_from_top`.
    let baseline_canvas_y = (metrics.cell_h as f64) - (metrics.baseline_from_top as f64);
    // Horizontal: CT's pen position lands at the glyph's logical
    // start; for a monospace font the ink left edge is at
    // `bbox.origin.x` from the pen.  We anchor the pen at canvas
    // x = -bbox.origin.x so the ink left edge falls exactly on
    // canvas x = 0 (left edge of the slot).  Side-bearing variations
    // (italic L overhang etc.) just shift the ink within the slot;
    // it's clipped to slot bounds.
    let origin = CGPoint::new(-bbox.origin.x, baseline_canvas_y);
    font.draw_glyphs(&[glyph], &[origin], ctx);

    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells,
    })
}

// Legacy per-glyph bearing fields (bearing_x / bearing_y) and the
// variable-sized bitmap they paired with were removed.  Slots are now
// uniformly cell-sized and every glyph's baseline lands at a fixed
// integer row inside the slot — see `rasterise_glyph` above.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_metal::system_default_device;
    use core_text::font::new_from_name;

    fn make_font() -> CTFont {
        new_from_name("Menlo", 13.0).expect("Menlo present on macOS")
    }

    fn test_metrics() -> SlotMetrics {
        // 16x32 cell with baseline 24 px from top — a plausible
        // Monaco-12-at-2x slot that tests don't depend on tightly.
        SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 }
    }

    #[test]
    fn rasterise_then_cache_hit() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => {
                eprintln!("skipping: no Metal device");
                return;
            }
        };
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let font = make_font();

        // Glyph for 'A'.
        let mut glyph: CGGlyph = 0;
        let cu: u16 = b'A' as u16;
        unsafe {
            font.get_glyphs_for_characters(&cu, &mut glyph, 1);
        }
        assert!(glyph != 0, "Menlo should have a glyph for 'A'");

        let key = GlyphKey { font_id: 0, glyph };
        let entry1 = atlas.get_or_rasterize(key, &font, test_metrics()).expect("first call rasterises");
        assert!(entry1.px_w > 0 && entry1.px_h > 0);
        assert_eq!(atlas.cache_len(), 1);

        let entry2 = atlas.get_or_rasterize(key, &font, test_metrics()).expect("second call from cache");
        assert_eq!(entry1.u0, entry2.u0, "second call must return the same UV");
        assert_eq!(atlas.cache_len(), 1, "cache must not grow on hit");
    }

    #[test]
    fn atlas_full_triggers_rebuild_and_keeps_serving() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        // Tiny atlas — every couple of glyphs forces a rebuild.  The
        // contract is "every individually-fit glyph eventually places
        // successfully" — silent skip would leave gaps in the cache.
        let mut atlas = GlyphAtlas::new(&device, 32, 32).expect("atlas");
        let font = make_font();

        let chars = b"abcdefghij";
        let mut placed = 0;
        for c in chars.iter() {
            let mut glyph: CGGlyph = 0;
            let cu: u16 = *c as u16;
            unsafe {
                font.get_glyphs_for_characters(&cu, &mut glyph, 1);
            }
            let key = GlyphKey { font_id: 0, glyph };
            if atlas.get_or_rasterize(key, &font, test_metrics()).is_some() {
                placed += 1;
            }
        }
        assert_eq!(placed, chars.len(), "every glyph must place via rebuild");
        assert!(
            atlas.rebuild_count > 0,
            "tiny atlas must have triggered at least one rebuild"
        );
    }
}
