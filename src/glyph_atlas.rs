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

use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_graphics::font::CGGlyph;
use core_graphics::geometry::CGPoint;
use core_text::font::CTFont;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLStorageMode, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use marspot_term::fast_hash::FxHashMap;
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

/// Reserved synthetic font_id for box-drawing / block-element glyphs
/// rasterised by our own code (not the CT font cache).  Chosen high
/// enough that real FontCache indices won't collide — FontCache grows
/// linearly from 0 as fallback fonts are discovered.
pub const BOX_DRAWING_FONT_ID: FontId = u32::MAX;

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
    /// font v5 (Phase 1) — per-glyph bearing from the typographic
    /// origin to the **ink box top-left** in pixel units, with the
    /// `PAD` left/top padding included in the bitmap.
    ///
    /// - `bearing_x` = `bbox.origin.x.floor()`.  For a typical
    ///   monospace cell-aligned glyph this is `0..3px`; for chrome
    ///   proportional glyphs it can swing.
    /// - `bearing_y` = `(bbox.origin.y + bbox.size.height).ceil()`.
    ///   Distance from baseline UP to ink top (positive = ink lives
    ///   above baseline, the normal case).  Descenders mean the
    ///   bitmap extends `px_h - bearing_y - 2*PAD` below baseline.
    ///
    /// Renderer formula:
    /// - chrome / proportional: ink_left = pen_x + bearing_x;
    ///   ink_top = baseline_y - bearing_y; quad = (ink_left - PAD,
    ///   ink_top - PAD, px_w, px_h)
    /// - mono PTY: ink_left forced to `cell_x` (typesetter
    ///   pre-balanced left bearing inside the cell); quad =
    ///   (cell_x - PAD, baseline_y - bearing_y - PAD, px_w, px_h)
    pub bearing_x: i16,
    pub bearing_y: i16,
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
    /// Bytes per pixel of the backing texture: 1 for the alpha-only
    /// (`R8Unorm`) mono atlas, 4 for the colour (`BGRA8Unorm`) atlas that
    /// holds full-colour emoji.  Drives the upload stride and which
    /// rasteriser `get_or_rasterize` dispatches to.
    bpp: u32,
    shelves: Vec<Shelf>,
    /// SipHash on a 6-byte (font_id u32, glyph u16) key was 1.7 % of
    /// L2 core CPU under the 9-active workload (sampled 2026-06-15);
    /// FxHash cuts that to a single mul+rotate per chunk.  Keys are
    /// derived from grid contents — never adversarial — so no DoS
    /// resistance is needed.
    cache: FxHashMap<GlyphKey, AtlasEntry>,
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
    /// Alpha-only (`R8Unorm`) atlas — the mono path for all text glyphs,
    /// tinted by the per-cell foreground colour in the FG shader.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        Self::with_format(device, width, height, false)
    }

    /// Colour (`BGRA8Unorm`) atlas — holds full-colour glyphs (Apple Color
    /// Emoji) sampled directly by the colour FG shader.  `color = true`
    /// switches the texture format + the upload stride + the rasteriser.
    pub fn new_color(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        Self::with_format(device, width, height, true)
    }

    fn with_format(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
    ) -> Result<Self, String> {
        let (format, bpp) = if color {
            (MTLPixelFormat::BGRA8Unorm, 4u32)
        } else {
            (MTLPixelFormat::R8Unorm, 1u32)
        };
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format,
                width as usize,
                height as usize,
                false,
            )
        };
        // Managed: CPU writes via replaceRegion, GPU reads.  On
        // Apple Silicon (UMA) Shared would also work and skip
        // the synchronize step, but Managed is portable across
        // Intel + Apple Silicon and the perf delta is irrelevant
        // for an atlas updated on cache miss only.
        descriptor.setStorageMode(MTLStorageMode::Managed);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        Ok(Self {
            texture,
            width,
            height,
            bpp,
            shelves: Vec::new(),
            cache: FxHashMap::default(),
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
        n_cells: u16,
    ) -> Option<AtlasEntry> {
        if let Some(&entry) = self.cache.get(&key) {
            return Some(entry);
        }
        let raster = if self.bpp == 4 {
            rasterise_glyph_color(font, key.glyph, metrics, n_cells)?
        } else {
            rasterise_glyph(font, key.glyph, metrics, n_cells)?
        };
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
            bearing_x: raster.bearing_x,
            bearing_y: raster.bearing_y,
        };
        self.cache.insert(key, entry);
        Some(entry)
    }

    /// Insert a caller-rastered 8-bit alpha mask under `key`.  Used
    /// for box-drawing / block-element characters: the font's CT-
    /// rasterised glyph for `─`/`│`/`╭` etc. is shorter than the cell
    /// advance, so blitting it as-is leaves visible gaps at borders.
    /// Callers (render_metal) detect those chars before this method
    /// and pass a rasteriser closure that fills a zero-initialised
    /// w×h byte buffer; the rest of the pipeline treats the entry
    /// like any other glyph — same shelf packer, same upload path,
    /// same textured-quad blit on the GPU.  Synthetic `key.font_id`
    /// should be chosen to avoid collision with real font ids (use
    /// `BOX_DRAWING_FONT_ID`).
    ///
    /// The closure form keeps the cache-hit path zero-alloc; only on
    /// miss do we allocate the rasterisation buffer.
    pub fn get_or_insert_custom_raster<F>(
        &mut self,
        key: GlyphKey,
        w: u32,
        h: u32,
        n_cells: u16,
        rasterise: F,
    ) -> Option<AtlasEntry>
    where
        F: FnOnce(&mut [u8]),
    {
        if let Some(&entry) = self.cache.get(&key) {
            return Some(entry);
        }
        let mut buf = vec![0u8; (w as usize) * (h as usize)];
        rasterise(&mut buf);
        let placed = match self.place(w, h) {
            Some(p) => p,
            None => {
                self.rebuild();
                self.place(w, h)?
            }
        };
        self.upload(&buf, w, h, placed.0, placed.1);
        // Box-drawing / block-element rasters are designed to TILE
        // the entire cell — caller fills (0,0)..(w,h) with the ink.
        // So the ink "origin" matches the slot top-left:
        //   bearing_x = 0  (ink left coincides with quad left)
        //   bearing_y = h  (ink top coincides with quad top — full
        //                   cell tall, no descender);  baseline math
        //                   becomes: quad_top = baseline - h, i.e.
        //                   the cell top.
        let entry = AtlasEntry {
            u0: placed.0 as u16,
            v0: placed.1 as u16,
            u1: (placed.0 + w) as u16,
            v1: (placed.1 + h) as u16,
            px_w: w as u16,
            px_h: h as u16,
            n_cells,
            bearing_x: 0,
            bearing_y: h as i16,
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
                    (w * self.bpp) as usize,
                );
        }
    }

    /// For tests / instrumentation.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Approximate resident bytes for instrumentation
    /// (MARSPOT_PROFILE_RSS).  Texture is reported at its full
    /// `width * height` R8 footprint (the renderer holds it via
    /// MTLTextureDescriptor::Managed, which keeps a CPU mirror), and
    /// the cache + shelves are reported at their `Vec`/`HashMap`
    /// capacities.  HashMap bucket overhead beyond the (key, value)
    /// pair size is approximated as one extra `usize` per bucket — a
    /// rough but stable proxy that lets slope analysis catch a leak
    /// in this subsystem without needing exact `std::collections`
    /// internals.
    pub fn approx_bytes(&self) -> usize {
        let texture_bytes = self.width as usize * self.height as usize * self.bpp as usize;
        let entry_bytes =
            std::mem::size_of::<GlyphKey>() + std::mem::size_of::<AtlasEntry>();
        let cache_bytes =
            self.cache.capacity() * (entry_bytes + std::mem::size_of::<usize>());
        let shelves_bytes = self.shelves.capacity() * std::mem::size_of::<Shelf>();
        texture_bytes + cache_bytes + shelves_bytes
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
    /// Per-glyph bearing — see [`AtlasEntry::bearing_x`].  Includes
    /// PAD offset bookkeeping so renderer can do a single subtract.
    bearing_x: i16,
    bearing_y: i16,
}

/// Rasterise one glyph into a CELL-SIZED alpha-only bitmap.  The
/// glyph's baseline is positioned at integer canvas y =
/// `metrics.cell_h - metrics.baseline_from_top` (y-up) — identical
/// for every glyph at the same font/size, so the renderer can place
/// every cell-sized slot at the cell origin and every baseline lines
/// up exactly.
///
/// Slot width is `n_cells` × `metrics.cell_w` — caller's responsibility
/// to pass the correct cell count (1 for ASCII, 2 for East Asian wide /
/// emoji per `grid::char_width`).  We deliberately do NOT infer from
/// the glyph's ink bbox: many CJK ideographs (e.g. 比/占/只) have
/// centred strokes whose bbox is narrower than 1.5× cell_w, which
/// would mis-classify them as 1-cell and render at half width while
/// the grid layer still reserves 2 cells.
///
/// Returns `None` only if the glyph has no ink (control char,
/// .notdef-with-degenerate-bbox).
fn rasterise_glyph(
    font: &CTFont,
    glyph: CGGlyph,
    metrics: SlotMetrics,
    n_cells: u16,
) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }

    let n_cells = n_cells.max(1);
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
    // Font smoothing ON: applies CoreText's stroke-widening for
    // gamma-correct AA.  We had this OFF briefly when the Metal
    // target was `BGRA8Unorm_sRGB`, because that did linear-space
    // blending which COMPOUNDED with smoothing's compensation and
    // made text read as "always bold".  With the target switched to
    // `BGRA8Unorm` (sRGB-space blending, the iTerm2 / Terminal.app
    // path), smoothing now lands at the designed weight — without it
    // glyphs feel ~0.5 px too thin / fragile.
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
    // Safety net: if the resolved font hands us a glyph whose natural
    // bbox is WIDER than the slot we allocated (1 cell wide for chars
    // grid::char_width says are 1-cell, etc.), scale the CTM to fit.
    // This catches the same "Apple-Color-Emoji-style em-box glyph in a
    // 1-cell slot" case that the colour rasteriser handles — but also
    // hits when the cascade lands on a TEXT font that happens to have
    // an oversized glyph (Helvetica's ② et al.).  scale==1.0 is a
    // no-op on the common path; only triggers when bbox > slot.
    let glyph_w = bbox.size.width;
    let glyph_h = bbox.size.height;
    let scale = if glyph_w > 0.0 && glyph_h > 0.0 {
        ((px_w as f64) / glyph_w)
            .min((px_h as f64) / glyph_h)
            .min(1.0)
    } else {
        1.0
    };
    let origin = if scale < 1.0 {
        ctx.scale(scale, scale);
        let user_w = (px_w as f64) / scale;
        let user_h = (px_h as f64) / scale;
        CGPoint::new(
            (user_w - glyph_w) / 2.0 - bbox.origin.x,
            (user_h - glyph_h) / 2.0 - bbox.origin.y,
        )
    } else {
        // Horizontal: CT's pen position lands at the glyph's logical
        // start; for a monospace font the ink left edge is at
        // `bbox.origin.x` from the pen.  We anchor the pen at canvas
        // x = -bbox.origin.x so the ink left edge falls exactly on
        // canvas x = 0 (left edge of the slot).
        CGPoint::new(-bbox.origin.x, baseline_canvas_y)
    };
    font.draw_glyphs(&[glyph], &[origin], ctx);

    // Phase 1.0 bearing fields — slot is still cell-aligned
    // (cell_w × n_cells, cell_h), so the renderer's "draw quad at
    // (cell_x, baseline_y - ascent), size cell_w × cell_h" formula
    // is exactly equivalent to:
    //   quad_left = cell_x + bearing_x      (with bearing_x = 0)
    //   quad_top  = baseline_y - bearing_y  (with bearing_y = baseline_from_top)
    // Both formulas land in the same place — Phase 1.0 records the
    // values so renderer can OPT IN to per-glyph placement later;
    // Phase 1.1 will switch atlas slots themselves to real bbox.
    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells,
        bearing_x: 0,
        bearing_y: metrics.baseline_from_top as i16,
    })
}

/// Rasterise one COLOUR glyph (Apple Color Emoji etc.) into a cell-sized
/// premultiplied-BGRA bitmap for the colour atlas.  Same slot geometry as
/// `rasterise_glyph` (baseline at `cell_h - baseline_from_top`, slot width
/// `n_cells × cell_w`), but the context is 4-channel BGRA with a device-RGB
/// colour space, so `draw_glyphs` emits the glyph's real colours (decoding
/// the embedded sbix bitmap) instead of an alpha mask.  Byte layout
/// (premultiplied-first + little-endian 32) is B,G,R,A in memory, matching
/// `MTLPixelFormat::BGRA8Unorm`; the colour FG shader samples it directly.
fn rasterise_glyph_color(
    font: &CTFont,
    glyph: CGGlyph,
    metrics: SlotMetrics,
    n_cells: u16,
) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }

    let n_cells = n_cells.max(1);
    let px_w = metrics.cell_w * n_cells as u32;
    let px_h = metrics.cell_h;

    let bytes_per_row = (px_w * 4) as usize;
    let buf_len = bytes_per_row * px_h as usize;
    let mut bytes: Vec<u8> = vec![0u8; buf_len];

    let cs = CGColorSpace::create_device_rgb();
    let bitmap_info = kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little;
    let ctx = unsafe {
        let raw = CGBitmapContextCreate(
            bytes.as_mut_ptr() as *mut c_void,
            px_w as usize,
            px_h as usize,
            8,
            bytes_per_row,
            cs.as_ptr() as *mut c_void,
            bitmap_info,
        );
        if raw.is_null() {
            return None;
        }
        CGContext::from_ptr(raw)
    };
    // Keep the colour space alive until the context has retained it.
    drop(cs);

    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    // No gray fill / smoothing knobs: a colour (sbix) glyph carries its own
    // pixels; CGTextFill draws the bitmap as-is.
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);

    // Scale-to-fit only when the glyph's natural bbox is LARGER than
    // the canvas (`scale < 1.0`).  This handles the case where a
    // non-Emoji_Presentation codepoint (① ②, certain Enclosed
    // Alphanumerics, etc.) fell through the text-font cascade to
    // CoreText's auto-discovery and landed on Apple Color Emoji,
    // which rasterises at em-box width — without scaling, the
    // 13.4pt-wide glyph clips into the 7pt-wide 1-cell canvas and
    // the user sees half a glyph.  For TRUE emoji (Emoji_Presentation
    // = Yes) `cluster_width` correctly assigns 2 cells, canvas is
    // ~14pt wide, ratio ≥ 1.0 and we don't scale.  Same for any
    // glyph that already fits — `scale == 1.0` is a no-op.
    let scale = ((px_w as f64) / bbox.size.width)
        .min((px_h as f64) / bbox.size.height)
        .min(1.0);
    let baseline_canvas_y = (metrics.cell_h as f64) - (metrics.baseline_from_top as f64);
    let origin = if scale < 1.0 {
        ctx.scale(scale, scale);
        // After CTM scale, the canvas occupies (px_w/scale, px_h/scale)
        // in user space.  Centre the glyph inside it so it reads as
        // "a smaller version of the same character", not pinned to
        // a corner.
        let user_w = (px_w as f64) / scale;
        let user_h = (px_h as f64) / scale;
        CGPoint::new(
            (user_w - bbox.size.width) / 2.0 - bbox.origin.x,
            (user_h - bbox.size.height) / 2.0 - bbox.origin.y,
        )
    } else {
        CGPoint::new(-bbox.origin.x, baseline_canvas_y)
    };
    font.draw_glyphs(&[glyph], &[origin], ctx);

    // Phase 1.0 bearing fields — see `rasterise_glyph` doc.  Color
    // emoji slot is also cell-aligned, so same constants apply.
    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells,
        bearing_x: 0,
        bearing_y: metrics.baseline_from_top as i16,
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
        let entry1 = atlas.get_or_rasterize(key, &font, test_metrics(), 1).expect("first call rasterises");
        assert!(entry1.px_w > 0 && entry1.px_h > 0);
        assert_eq!(atlas.cache_len(), 1);

        let entry2 = atlas.get_or_rasterize(key, &font, test_metrics(), 1).expect("second call from cache");
        assert_eq!(entry1.u0, entry2.u0, "second call must return the same UV");
        assert_eq!(atlas.cache_len(), 1, "cache must not grow on hit");
    }

    #[test]
    fn atlas_full_triggers_rebuild_and_keeps_serving() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        // Atlas just big enough for ~8 cell-sized slots (4 wide × 2
        // tall, given test_metrics cell_w=16, cell_h=32, PAD=1).
        // Feeding 10 chars therefore forces a rebuild + a couple of
        // post-rebuild placements.  The contract is "every
        // individually-fit glyph eventually places successfully" —
        // silent skip would leave gaps in the cache.
        let m = test_metrics();
        let atlas_w = (m.cell_w + 2 * PAD) * 4;
        let atlas_h = (m.cell_h + 2 * PAD) * 2;
        let mut atlas = GlyphAtlas::new(&device, atlas_w, atlas_h).expect("atlas");
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
            if atlas.get_or_rasterize(key, &font, test_metrics(), 1).is_some() {
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
