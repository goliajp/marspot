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

/// Phase 2 — atlas lookup key.
///
/// `(font_id, glyph)` no longer suffice once multiple text sizes share
/// an atlas: 12-pt PTY and 13-pt chrome both ask for `glyph_id = 36`
/// of Monaco and would stomp each other's bitmap.  Add `size_q`
/// (round(pt × 4) — 0.25-pt buckets) so each size carries its own
/// slot.  `subpx_x` is reserved (set 0 in Phase 2; Phase 4 will fill
/// it with `0..4` to encode sub-pixel x-bucket).  `flags` records the
/// rasteriser knobs that actually change the bitmap — currently
/// `FLAG_SMOOTH` for font_smoothing on (default for text) and
/// `FLAG_SUBPX_AA` reserved for future macOS-Intel paths (off on
/// Apple Silicon, where the atlas target is `BGRA8Unorm` and CT
/// emits grayscale-AA).
///
/// Key is `font_id (4B) + glyph (2B) + size_q (2B) + subpx_x (1B) +
/// flags (1B)` = 10 bytes packed — still cheap to hash via FxHash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey {
    pub font_id: FontId,
    pub glyph: CGGlyph,
    pub size_q: u16,
    pub subpx_x: u8,
    pub flags: u8,
}

impl GlyphKey {
    /// Bit 0 — `set_should_smooth_fonts(true)` (CT stroke-widening for
    /// gamma-correct AA).  Default for text rasters.
    pub const FLAG_SMOOTH: u8 = 1 << 0;

    /// Bit 1 — true sub-pixel AA (LCD-style RGB stripe ordering).
    /// Reserved; the atlas target on Apple Silicon is `BGRA8Unorm`
    /// where CT emits grayscale AA, so this is off in all current
    /// call paths.  Distinguished from `FLAG_SMOOTH` so a future
    /// macOS-Intel build can opt in without invalidating the key
    /// shape.
    pub const FLAG_SUBPX_AA: u8 = 1 << 1;

    /// Quantise a `pt` size to a 0.25-pt bucket.  Two sizes that
    /// round to the same `size_q` share an atlas slot.
    #[inline]
    pub fn size_q_for(pt: f64) -> u16 {
        (pt * 4.0).round() as u16
    }
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

impl AtlasEntry {
    /// Per Phase 1.1 bearing formula — given the typographic pen
    /// position (`pen_x` = slot left edge, `baseline_y` = on-screen
    /// baseline row, both in physical px), return the quad's
    /// `(origin, size)`.  Used at every `GlyphInstance` push site so
    /// every renderer sub-system shares the same geometry.
    ///
    /// For Phase 1.0 entries (bearing_x = 0, bearing_y = ascent,
    /// `px_w`/`px_h` = cell dims) this yields `(pen_x, baseline_y -
    /// ascent)` with cell-sized quad — bitwise equivalent to the
    /// pre-Phase-1.1 "draw cell-sized slot at cell origin" formula.
    ///
    /// After Phase 1.1 the CT rasteriser produces bbox-sized bitmaps
    /// with 1-px PAD round each side (lsb pre-cancelled, so
    /// `bearing_x = -PAD = -1`, `bearing_y = ink_ascent + PAD`).  The
    /// formula then places the bitmap so the ink interior lands at
    /// `(pen_x, baseline_y - ink_ascent)` — identical screen position
    /// to the cell-sized layout for monospace ASCII (Monaco lsb ≈ 0),
    /// only the surrounding empty space is smaller.
    #[inline]
    pub fn quad(self, pen_x: f32, baseline_y: f32) -> ([f32; 2], [f32; 2]) {
        (
            [pen_x + self.bearing_x as f32, baseline_y - self.bearing_y as f32],
            [self.px_w as f32, self.px_h as f32],
        )
    }
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
        baseline_from_top: u32,
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
        // Box-drawing / block-element rasters TILE the entire cell —
        // caller fills (0,0)..(w,h) with ink.  The slot is meant to
        // land at (cell_x, cell_top) with no padding.
        //   bearing_x = 0          — slot left  = cell_x  = pen_x
        //   bearing_y = ascent     — slot top   = cell_top = baseline_y - ascent
        // Renderer formula `quad_top = baseline_y - bearing_y` then
        // yields quad_top = cell_top exactly.
        let entry = AtlasEntry {
            u0: placed.0 as u16,
            v0: placed.1 as u16,
            u1: (placed.0 + w) as u16,
            v1: (placed.1 + h) as u16,
            px_w: w as u16,
            px_h: h as u16,
            n_cells,
            bearing_x: 0,
            bearing_y: baseline_from_top as i16,
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

/// Rasterise one glyph into an **alpha-only bitmap sized to the
/// glyph's real ink bbox** plus 1 px PAD on every side (Phase 1.1).
///
/// Pen position cancels the font's left side bearing, so the ink's
/// left edge lands at bitmap_x = `PAD` regardless of font.  The pen
/// y likewise places the ink top at bitmap_y_down = `PAD`.  Reported
/// `bearing_x = -PAD` and `bearing_y = ceil(ink_ascent) + PAD` carry
/// the PAD offset, so the renderer's `quad = (pen_x + bearing_x,
/// baseline_y - bearing_y, px_w, px_h)` formula puts the ink interior
/// at exactly `(pen_x, baseline_y - ink_ascent)` — i.e. the typographic
/// position — bit-equivalent (modulo sub-pixel AA rounding) to the
/// pre-Phase-1.1 layout where the glyph was drawn into a cell-sized
/// slot with baseline at integer `baseline_from_top`.
///
/// Oversized-glyph safety net: if the natural bbox would exceed the
/// caller's slot (`n_cells × cell_w` by `cell_h`) — e.g. the text-font
/// cascade falls onto Apple-Color-Emoji-shaped glyphs (`②`, certain
/// enclosed alphanumerics) — fall back to the cell-aligned scale-to-fit
/// path so the glyph doesn't spill into neighbouring cells.  This case
/// reports Phase 1.0 bearing values (`bearing_x = 0`, `bearing_y =
/// baseline_from_top`) so the renderer paints a cell-sized quad at the
/// cell origin, matching the legacy behaviour exactly.
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
    let slot_w = metrics.cell_w * n_cells as u32;
    let slot_h = metrics.cell_h;
    let glyph_w = bbox.size.width;
    let glyph_h = bbox.size.height;

    // Pick path: oversized glyph → scale-to-fit cell-sized bitmap
    // (legacy); otherwise → Phase 1.1 bbox-sized bitmap with PAD.
    let oversized = glyph_w > slot_w as f64 || glyph_h > slot_h as f64;
    let (px_w, px_h) = if oversized {
        (slot_w, slot_h)
    } else {
        (
            glyph_w.ceil() as u32 + 2 * PAD,
            glyph_h.ceil() as u32 + 2 * PAD,
        )
    };

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

    if oversized {
        // Legacy: scale the CTM so the glyph fits the cell, centre it,
        // and report Phase 1.0 bearings so the renderer paints a
        // cell-sized quad at the cell origin.
        let scale = ((px_w as f64) / glyph_w)
            .min((px_h as f64) / glyph_h)
            .min(1.0);
        ctx.scale(scale, scale);
        let user_w = (px_w as f64) / scale;
        let user_h = (px_h as f64) / scale;
        let origin = CGPoint::new(
            (user_w - glyph_w) / 2.0 - bbox.origin.x,
            (user_h - glyph_h) / 2.0 - bbox.origin.y,
        );
        font.draw_glyphs(&[glyph], &[origin], ctx);
        return Some(Raster {
            bytes,
            px_w,
            px_h,
            n_cells,
            bearing_x: 0,
            bearing_y: metrics.baseline_from_top as i16,
        });
    }

    // Phase 1.1 natural path.  Pen cancels the font's lsb so the ink
    // left edge lands at bitmap_x = PAD.  Pen y in canvas (y-up) is
    // chosen so the ink TOP lands at bitmap_y_down = PAD, i.e.
    // bitmap_y_up = px_h - PAD.  That makes the in-bitmap baseline
    // an integer row (`PAD + ceil(ink_ascent)`), so cross-glyph
    // baselines line up exactly when the renderer aligns origin.y
    // via the bearing formula.
    let pen_x = (PAD as f64) - bbox.origin.x;
    let pen_y = (px_h as f64) - (PAD as f64) - bbox.origin.y - bbox.size.height;
    font.draw_glyphs(&[glyph], &[CGPoint::new(pen_x, pen_y)], ctx);

    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells,
        // lsb pre-cancelled at raster time; `bearing_x = -PAD` slides
        // the quad 1 px left so the renderer's `pen_x + bearing_x +
        // ink_left_in_bitmap` lands at `pen_x` exactly.  `bearing_y`
        // measures baseline up to bitmap TOP (= ink_ascent + PAD).
        bearing_x: -(PAD as i16),
        bearing_y: ((bbox.origin.y + bbox.size.height).ceil() as i16) + (PAD as i16),
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
    let slot_w = metrics.cell_w * n_cells as u32;
    let slot_h = metrics.cell_h;
    let glyph_w = bbox.size.width;
    let glyph_h = bbox.size.height;

    // Mirrors the mono path's split: oversized glyph (the typical
    // Apple Color Emoji case — em-box bitmap larger than the cell) →
    // cell-sized + scale-to-fit; otherwise → Phase 1.1 bbox-sized
    // bitmap with PAD.  Emoji that legitimately occupy 2 cells (true
    // `Emoji_Presentation` rendering at 16×32 cell × 2 ≈ em box) take
    // the oversized branch and keep current behaviour.
    let oversized = glyph_w > slot_w as f64 || glyph_h > slot_h as f64;
    let (px_w, px_h) = if oversized {
        (slot_w, slot_h)
    } else {
        (
            glyph_w.ceil() as u32 + 2 * PAD,
            glyph_h.ceil() as u32 + 2 * PAD,
        )
    };

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

    if oversized {
        let scale = ((px_w as f64) / glyph_w)
            .min((px_h as f64) / glyph_h)
            .min(1.0);
        ctx.scale(scale, scale);
        let user_w = (px_w as f64) / scale;
        let user_h = (px_h as f64) / scale;
        let origin = CGPoint::new(
            (user_w - glyph_w) / 2.0 - bbox.origin.x,
            (user_h - glyph_h) / 2.0 - bbox.origin.y,
        );
        font.draw_glyphs(&[glyph], &[origin], ctx);
        return Some(Raster {
            bytes,
            px_w,
            px_h,
            n_cells,
            bearing_x: 0,
            bearing_y: metrics.baseline_from_top as i16,
        });
    }

    // Phase 1.1 natural path — see `rasterise_glyph` for the geometry
    // derivation; identical placement, different bitmap pixel format.
    let pen_x = (PAD as f64) - bbox.origin.x;
    let pen_y = (px_h as f64) - (PAD as f64) - bbox.origin.y - bbox.size.height;
    font.draw_glyphs(&[glyph], &[CGPoint::new(pen_x, pen_y)], ctx);

    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells,
        bearing_x: -(PAD as i16),
        bearing_y: ((bbox.origin.y + bbox.size.height).ceil() as i16) + (PAD as i16),
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

        let key = GlyphKey {
            font_id: 0,
            glyph,
            size_q: GlyphKey::size_q_for(13.0),
            subpx_x: 0,
            flags: GlyphKey::FLAG_SMOOTH,
        };
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
        // Sized to force at least one shelf rebuild with Phase-1.1
        // bbox-sized bitmaps (ASCII glyphs at Menlo 13 are ~6×10 px
        // including PAD instead of 16×32 cell-sized).  A 32×32 atlas
        // fits a handful of small ASCII shelves; 10 chars overflow
        // the height once shelves close — the contract is "every
        // individually-fit glyph eventually places successfully", so
        // silent skip would leave gaps in the cache.
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
            let key = GlyphKey {
            font_id: 0,
            glyph,
            size_q: GlyphKey::size_q_for(13.0),
            subpx_x: 0,
            flags: GlyphKey::FLAG_SMOOTH,
        };
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
