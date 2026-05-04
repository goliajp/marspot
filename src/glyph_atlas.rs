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
/// in the atlas (`u0/v0/u1/v1`, in atlas pixel coords), its pixel
/// size, and the offset from the cell's baseline-origin to the
/// bitmap's top-left.
#[derive(Clone, Copy, Debug)]
pub struct AtlasEntry {
    pub u0: u16,
    pub v0: u16,
    pub u1: u16,
    pub v1: u16,
    pub px_w: u16,
    pub px_h: u16,
    /// Bitmap-top offset from the cell's pen-position baseline.
    /// Stored as f32 (not i16) so glyphs whose ideal bearing lands
    /// near a half-pixel boundary don't quantise to different
    /// integer values: `l` rounding to 11 and `d` rounding to 10
    /// pulled `l` 1 px higher than its row-mates and showed up
    /// visually as "develop's l/p sit lower than the rest" (Monaco
    /// 12 surfaced this; Menlo 13's metrics happened to round
    /// uniformly).  Renderers do their own round-to-pixel at draw
    /// time if they need crisp edges.
    pub bearing_x: f32,
    pub bearing_y: f32,
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
    /// `Some(entry)` on success; `None` only if the glyph itself has no
    /// ink (control char, .notdef-with-degenerate-bbox) or is wider
    /// than the entire atlas (pathological).  When the shelf packer
    /// runs out of room we atomically rebuild and retry — see the
    /// module-level "Eviction" doc.
    pub fn get_or_rasterize(
        &mut self,
        key: GlyphKey,
        font: &CTFont,
    ) -> Option<AtlasEntry> {
        if let Some(&entry) = self.cache.get(&key) {
            return Some(entry);
        }
        let raster = rasterise_glyph(font, key.glyph)?;
        let placed = match self.place(raster.px_w, raster.px_h) {
            Some(p) => p,
            None => {
                // Atlas full.  Rebuild atomically and retry — next
                // frame's render call will re-rasterise whichever
                // glyphs are still on screen.
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
            bearing_x: raster.bearing_x,
            bearing_y: raster.bearing_y,
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

/// Output of the CT-rasterise pass: alpha bytes + size + bearing.
struct Raster {
    bytes: Vec<u8>,
    px_w: u32,
    px_h: u32,
    bearing_x: f32,
    bearing_y: f32,
}

/// Rasterise one glyph into an alpha-only bitmap and return the bytes.
/// Returns `None` if the glyph has no ink (e.g. .notdef → bbox is
/// degenerate, or a control char).
fn rasterise_glyph(font: &CTFont, glyph: CGGlyph) -> Option<Raster> {
    // Ask CT for the glyph's bounding box in points.  This is the
    // minimum rectangle the rasterised glyph fits in — we add 1 px
    // of slack on each side to stay clear of subpixel-positioning
    // overflow.
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }

    let slack: CGFloat = 1.0;
    let px_w = (bbox.size.width.ceil() as u32) + 2 * slack as u32;
    let px_h = (bbox.size.height.ceil() as u32) + 2 * slack as u32;
    let bytes_per_row = px_w as usize;
    let buf_len = bytes_per_row * px_h as usize;
    let mut bytes: Vec<u8> = vec![0u8; buf_len];

    // Build an alpha-only CGBitmapContext over our own buffer.
    // colorSpace = NULL is required for AlphaOnly.  CGBitmapContextCreate
    // returns +1 retained, so wrap with `from_ptr` (takes ownership) — not
    // `from_existing_context_ptr` (which would over-retain).
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

    // Apple's standard "draw with full hinting" knobs — same as the
    // AppKit renderer in render.rs.  Keeps glyph appearance consistent
    // across the two renderers during the A/B phase.
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_should_smooth_fonts(true);
    ctx.set_allows_font_smoothing(true);
    ctx.set_should_subpixel_position_fonts(true);
    ctx.set_allows_font_subpixel_positioning(true);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    // White fill = 100 % alpha coverage in alpha-only context.
    ctx.set_gray_fill_color(1.0, 1.0);

    // Position the glyph so its bbox bottom-left lands at (slack, slack).
    let origin = CGPoint::new(slack - bbox.origin.x, slack - bbox.origin.y);
    font.draw_glyphs(&[glyph], &[origin], ctx);

    Some(Raster {
        bytes,
        px_w,
        px_h,
        // bearing_x = where the glyph's left edge is relative to the
        // pen position the renderer uses to lay out the cell.  Float
        // so half-pixel offsets stay consistent across all glyphs in
        // the same row (no per-glyph round-to-int drift).
        bearing_x: (bbox.origin.x - slack) as f32,
        // bearing_y = bitmap rows from buffer-top down to the glyph
        // baseline.  CGBitmapContext stores y-up internally, but the
        // BUFFER bytes are written top-down — buffer row 0 = canvas
        // top.  CT places baseline at canvas y = `slack - bbox.origin.y`
        // (so the descender bottom lands at canvas y = slack and the
        // top of the glyph ink lands at canvas y =
        // `slack + bbox.size.height + bbox.origin.y` ≤ px_h).
        // Therefore baseline lives at buffer row
        // `(px_h - 1) - (slack - bbox.origin.y)`.
        //
        // This matters because px_h = `ceil(bbox.size.height) + 2*slack`
        // (an integer) while `bbox.size.height` is fractional; the old
        // formula `bbox.origin.y + bbox.size.height + slack` lost the
        // `ceil`-padding above the glyph, so glyphs with the same
        // numerical sum but different px_h (e.g. 'p' descender + 'o'
        // x-only) ended up at the same dest_y and their baselines
        // drifted apart by a fraction of a pixel — visible as "p sits
        // lower than o" in 9-grid Monaco 12 prompts.
        bearing_y: (px_h as f32 - 1.0) - (slack as f32 - bbox.origin.y as f32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_metal::system_default_device;
    use core_text::font::new_from_name;

    fn make_font() -> CTFont {
        new_from_name("Menlo", 13.0).expect("Menlo present on macOS")
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
        let entry1 = atlas.get_or_rasterize(key, &font).expect("first call rasterises");
        assert!(entry1.px_w > 0 && entry1.px_h > 0);
        assert_eq!(atlas.cache_len(), 1);

        let entry2 = atlas.get_or_rasterize(key, &font).expect("second call from cache");
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
            if atlas.get_or_rasterize(key, &font).is_some() {
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
