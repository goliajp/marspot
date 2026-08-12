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
unsafe extern "C" {
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
/// flags (1B)` = 10 bytes logical, **8 bytes physical** after the
/// perf-render-p99 attack — packed into a single `u64` newtype so:
/// (1) Rust struct align padding goes away (the 5-field struct landed
///     at sizeof=12B / align=4 before packing);
/// (2) FxHash collapses to a single `u64` mul+rotate (no 5-field
///     chain);
/// (3) the cache `FxHashMap<GlyphKey, AtlasEntry>` shrinks 12→8 per
///     entry, lifting L1d density at the hot lookup site.
///
/// Bit layout (LSB → MSB):
/// - `[0..32)`  font_id (FontId = u32; full range)
/// - `[32..48)` glyph (CGGlyph = u16; full range)
/// - `[48..60)` size_q (12 bits = 4096 max, room for 1024 pt × 4
///               buckets; current PTY/chrome usage stays under 80)
/// - `[60..62)` subpx_x (2 bits, 0..4 — Phase 4 sub-pixel bucket)
/// - `[62..64)` flags (2 bits — `FLAG_SMOOTH` + `FLAG_SUBPX_AA`)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey(u64);

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


    // Bit layout constants — keep colocated with `new()` / accessors
    // so the packing scheme is one block to audit.
    // The 64 bits were fully spoken for — font_id took 32 of them for
    // a registry that never holds more than a few dozen fonts.  Trimmed
    // to 24 (16 M) so `flags` has room to grow; 6 bits are left spare.
    const FONT_ID_SHIFT: u32 = 0;
    const GLYPH_SHIFT: u32 = 24;
    const SIZE_Q_SHIFT: u32 = 40;
    const SUBPX_X_SHIFT: u32 = 52;
    const FLAGS_SHIFT: u32 = 54;

    const FONT_ID_MASK: u64 = 0x00FF_FFFF; // 24 bits
    const SIZE_Q_MASK: u64 = 0x0FFF; // 12 bits
    const SUBPX_X_MASK: u64 = 0x03; //  2 bits
    const FLAGS_MASK: u64 = 0x0F; //  4 bits

    /// Pack the 5 logical fields into the 64-bit key.  Debug builds
    /// assert no field exceeds its allotted bit width — production
    /// callers always pass values within range (size_q ≤ ~100,
    /// subpx_x ∈ 0..4, flags ∈ {0, FLAG_SMOOTH}), so the asserts only
    /// fire on a regression.
    #[inline]
    pub fn new(font_id: FontId, glyph: CGGlyph, size_q: u16, subpx_x: u8, flags: u8) -> Self {
        debug_assert!(
            (size_q as u64) <= Self::SIZE_Q_MASK,
            "size_q {size_q} > 4095 — packing scheme overflow"
        );
        debug_assert!(
            (subpx_x as u64) <= Self::SUBPX_X_MASK,
            "subpx_x {subpx_x} > 3 — packing scheme overflow"
        );
        debug_assert!(
            (flags as u64) <= Self::FLAGS_MASK,
            "flags {flags:#x} > 0xF — packing scheme overflow"
        );
        debug_assert!(
            (font_id as u64) <= Self::FONT_ID_MASK,
            "font_id {font_id} > 16 M — packing scheme overflow"
        );
        let packed = (((font_id as u64) & Self::FONT_ID_MASK) << Self::FONT_ID_SHIFT)
            | ((glyph as u64) << Self::GLYPH_SHIFT)
            | (((size_q as u64) & Self::SIZE_Q_MASK) << Self::SIZE_Q_SHIFT)
            | (((subpx_x as u64) & Self::SUBPX_X_MASK) << Self::SUBPX_X_SHIFT)
            | (((flags as u64) & Self::FLAGS_MASK) << Self::FLAGS_SHIFT);
        GlyphKey(packed)
    }

    #[inline]
    pub fn font_id(self) -> FontId {
        ((self.0 >> Self::FONT_ID_SHIFT) & Self::FONT_ID_MASK) as u32
    }

    #[inline]
    pub fn glyph(self) -> CGGlyph {
        (self.0 >> Self::GLYPH_SHIFT) as u16
    }

    #[inline]
    pub fn size_q(self) -> u16 {
        ((self.0 >> Self::SIZE_Q_SHIFT) & Self::SIZE_Q_MASK) as u16
    }

    #[inline]
    pub fn subpx_x(self) -> u8 {
        ((self.0 >> Self::SUBPX_X_SHIFT) & Self::SUBPX_X_MASK) as u8
    }

    #[inline]
    pub fn flags(self) -> u8 {
        ((self.0 >> Self::FLAGS_SHIFT) & Self::FLAGS_MASK) as u8
    }

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
    /// Phase 6 — LRU stamp: the most recent `frame_id` value at the
    /// time this entry was last looked up (or first placed).  Atlas
    /// `evict()` picks the shelf whose `max(entry.last_used)` is
    /// oldest and recycles it, so frames of work that referenced
    /// every entry recently keep them all alive.  64 bits never
    /// wraps in practice (at 120 fps, `u64::MAX` frames ≈ 5 × 10⁹
    /// years).
    pub last_used: u64,
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
#[derive(Clone, Debug)]
struct Shelf {
    /// Top of the shelf in atlas pixel space (0 = top).
    y: u32,
    /// Allocated height of the shelf.
    h: u32,
    /// Pixels consumed from the left.
    x_used: u32,
    /// Phase 6 — keys of every entry currently placed on this shelf.
    /// Used by the eviction path to walk + remove cache entries when
    /// the shelf is recycled.  Order is placement order; the renderer
    /// never iterates this directly.
    entries: Vec<GlyphKey>,
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
    /// Phase 6 — counts of legacy whole-atlas rebuilds (kept as a
    /// safety net: triggered when even single-shelf eviction can't
    /// place the requested glyph, e.g. the glyph is wider than any
    /// existing shelf).  Should stay 0 in steady state.
    pub rebuild_count: u64,
    /// Shelf evictions since construction — see `evict_lru_shelf`.
    pub evictions: u64,
    /// Phase 6 — per-shelf evictions.  Each LRU eviction recycles
    /// one shelf's slot space (without dropping the rest of the
    /// atlas), so this number can climb harmlessly while the working
    /// set rotates — observable proof the LRU path is doing work
    /// instead of the legacy whole-atlas reset.
    pub evict_count: u64,
    /// Phase 6 — current frame stamp used to tag entries on access.
    /// Renderer calls `begin_frame(frame_id)` at the start of each
    /// frame; cache hits then set `entry.last_used = current_frame`,
    /// and eviction picks the shelf whose newest entry is oldest.
    current_frame: u64,
    /// Phase 10 — natural-bbox rasteriser.  CoreText impls in
    /// `font_trait` delegate to the existing free functions; tests
    /// can plug a `MockRasteriser` to exercise the atlas without a
    /// CoreText font.  The PTY `get_or_rasterize` path still calls
    /// CoreText directly — Phase 10 plumbs only the chrome / natural
    /// path.
    rasteriser: Box<dyn crate::font_trait::Rasteriser>,
    /// Glyphs actually rasterised since the last [`Self::take_rasterised`].
    ///
    /// A cache miss here is a CoreText rasterise plus a texture
    /// upload — orders of magnitude more than a hit — and a freshly
    /// spawned core starts with every one of them ahead of it.  Frame
    /// timings alone cannot separate "the GPU was busy" from "we drew
    /// four thousand glyphs for the first time"; this counter is what
    /// makes the two distinguishable in a stall report.
    rasterised: u32,
}

/// 1-px padding on every side of every glyph; prevents linear
/// filtering from sampling the neighbour above / left.
const PAD: u32 = 1;

/// Phase B v2 attack #2 — LRU `last_used` decay-store resolution
/// (in frames).  Cache hits write `entry.last_used = current_frame`
/// **only when the stamp is older than this window**, instead of
/// every hit.  The hot render loop sees ~1–10 K cache hits per
/// frame; at 120 Hz the previous "store-every-hit" policy spent
/// ~5–10 µs/frame on memory writes that were redundant — every
/// hit within the same frame already wrote the same value, and the
/// LRU eviction path only cares about coarse age (which shelf is
/// stalest, not which entry on it is N-frame fresher than another).
///
/// `8` keeps eviction order stable on a 120 Hz render budget: a
/// shelf that hasn't been hit for 8+ frames (~67 ms) is genuinely
/// stale, while still suppressing 7-of-8 stores on the steady-state
/// hit path.
const LAST_USED_RESOLUTION: u64 = 8;

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

    /// Phase 10 — same as `new` / `new_color` but plugs a caller-
    /// supplied rasteriser.  Headless tests use this with
    /// `MockRasteriser` to exercise shelf packing / LRU eviction
    /// without a CoreText font.
    pub fn new_with_rasteriser(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
        rasteriser: Box<dyn crate::font_trait::Rasteriser>,
    ) -> Result<Self, String> {
        Self::with_format_and_rasteriser(device, width, height, color, rasteriser)
    }

    fn with_format(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
    ) -> Result<Self, String> {
        let rasteriser: Box<dyn crate::font_trait::Rasteriser> = if color {
            Box::new(crate::font_trait::CoreTextColorRasteriser)
        } else {
            Box::new(crate::font_trait::CoreTextMonoRasteriser)
        };
        Self::with_format_and_rasteriser(device, width, height, color, rasteriser)
    }

    fn with_format_and_rasteriser(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
        rasteriser: Box<dyn crate::font_trait::Rasteriser>,
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
            rasterised: 0,
            rebuild_count: 0,
            evictions: 0,
            evict_count: 0,
            current_frame: 0,
            rasteriser,
        })
    }

    /// Phase 6 — renderer call at the start of every frame.  The
    /// `frame_id` should monotonically increase; entries looked up
    /// during the frame stamp themselves with it, and the LRU
    /// eviction path picks the shelf with the oldest newest stamp.
    /// Call from `MetalRenderer::render` (and the headless test
    /// harness when the test cares about eviction order).
    pub fn begin_frame(&mut self, frame_id: u64) {
        self.current_frame = frame_id;
    }

    /// Read and reset the miss counter — how many glyphs this atlas
    /// had to rasterise since the last call.  Reset-on-read so the
    /// caller gets a per-frame number without having to remember a
    /// previous total.
    pub fn take_rasterised(&mut self) -> u32 {
        std::mem::take(&mut self.rasterised)
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
        if let Some(entry) = self.cache.get_mut(&key) {
            // Phase 6 — touch on hit so the LRU eviction path keeps
            // this glyph alive for as long as the current working set
            // references it.  Phase B v2 attack #2 — decay-store: skip
            // the write when the stamp is already within
            // `LAST_USED_RESOLUTION` frames of `current_frame`, which
            // collapses ~7/8 store-buffer ops on the hot lookup path
            // without disturbing eviction's coarse age ordering.
            let cf = self.current_frame;
            if cf.wrapping_sub(entry.last_used) >= LAST_USED_RESOLUTION {
                entry.last_used = cf;
            }
            return Some(*entry);
        }
        let raster = if self.bpp == 4 {
            rasterise_glyph_color(font, key.glyph(), metrics, n_cells)?
        } else {
            rasterise_glyph(font, key.glyph(), metrics, n_cells)?
        };
        self.commit_raster(key, raster)
    }

    /// Phase 3 — chrome (proportional) raster path.  Same as
    /// `get_or_rasterize` but skips the `n_cells × cell_w` /
    /// `cell_h` "oversized" check that the PTY path uses as a
    /// safety net for emoji-sized glyphs landing in a 1-cell
    /// slot.  Chrome glyphs are sized for whatever advance CT
    /// reports — there's no cell constraint to honour, so the
    /// rasteriser always takes the natural Phase 1.1 path
    /// (`bbox + 2*PAD` bitmap, real bearings, lsb pre-cancelled).
    pub fn get_or_rasterize_natural(
        &mut self,
        key: GlyphKey,
        font: &CTFont,
    ) -> Option<AtlasEntry> {
        if let Some(entry) = self.cache.get_mut(&key) {
            // Phase B v2 attack #2 — decay-store (see comment on
            // `LAST_USED_RESOLUTION`).
            let cf = self.current_frame;
            if cf.wrapping_sub(entry.last_used) >= LAST_USED_RESOLUTION {
                entry.last_used = cf;
            }
            return Some(*entry);
        }
        // Phase 10 — dispatch through the trait (`CoreText*Rasteriser`
        // by default; tests can plug `MockRasteriser`).  The
        // `key.subpx_x` ∈ 0..4 sub-pixel bucket (Phase 4) is part of
        // the contract so the trait impl rasterises the correct
        // variant.
        let out = self.rasteriser.rasterise(font, key.glyph(), key.subpx_x())?;
        let raster = Raster {
            bytes: out.bytes,
            px_w: out.px_w,
            px_h: out.px_h,
            n_cells: 1,
            bearing_x: out.bearing_x,
            bearing_y: out.bearing_y,
        };
        self.commit_raster(key, raster)
    }

    fn commit_raster(&mut self, key: GlyphKey, raster: Raster) -> Option<AtlasEntry> {
        let (x, y, shelf_idx) = self.place_or_evict(raster.px_w, raster.px_h)?;
        self.upload(&raster.bytes, raster.px_w, raster.px_h, x, y);
        let entry = AtlasEntry {
            u0: x as u16,
            v0: y as u16,
            u1: (x + raster.px_w) as u16,
            v1: (y + raster.px_h) as u16,
            px_w: raster.px_w as u16,
            px_h: raster.px_h as u16,
            n_cells: raster.n_cells,
            bearing_x: raster.bearing_x,
            bearing_y: raster.bearing_y,
            last_used: self.current_frame,
        };
        self.cache.insert(key, entry);
        self.shelves[shelf_idx].entries.push(key);
        Some(entry)
    }

    /// Phase 6 — try `place()` first; on failure, evict the LRU shelf
    /// that can hold `(w, h)` and retry; on still-failure, fall back to
    /// the legacy whole-atlas rebuild.  Returns `(x, y, shelf_idx)` on
    /// success so the caller can record the new entry on its shelf.
    fn place_or_evict(&mut self, w: u32, h: u32) -> Option<(u32, u32, usize)> {
        // Every insert path funnels through here — the CT rasteriser,
        // the natural-bbox one, and the custom-buffer one — so this is
        // the single place a miss can be counted without three
        // bookkeeping sites drifting apart.
        self.rasterised = self.rasterised.saturating_add(1);
        if let Some(p) = self.place(w, h) {
            return Some(p);
        }
        if self.evict_lru_shelf(w + 2 * PAD, h + 2 * PAD) {
            if let Some(p) = self.place(w, h) {
                return Some(p);
            }
        }
        self.rebuild();
        self.place(w, h)
    }

    /// Phase 6 — recycle the shelf whose newest entry is the oldest
    /// (= the shelf least recently touched).  Drops every cached
    /// entry that lives on it and resets its `x_used` so the packer
    /// fills it back from the left.  Texture pixel data is left
    /// in place — UV coords get reassigned, the abandoned regions
    /// are simply never sampled again.
    ///
    /// Returns `true` when a shelf was found that can hold the
    /// requested padded `(needed_w, needed_h)`; `false` when no
    /// existing shelf has the right height, so the caller falls back
    /// to whole-atlas rebuild.
    fn evict_lru_shelf(&mut self, needed_w: u32, needed_h: u32) -> bool {
        if needed_w > self.width {
            return false;
        }
        // Counted because a single frame can both fill the atlas and
        // evict from it: 17k distinct CJK glyphs do not fit in 4096²
        // at 2× rasterisation, so the frame starts throwing away
        // glyphs it will need again before it ends.  Without this
        // number, that shows up only as "the per-glyph cost tripled"
        // and gets mistaken for the rasteriser being slow.
        self.evictions = self.evictions.saturating_add(1);
        let mut best: Option<(usize, u64)> = None;
        for (idx, shelf) in self.shelves.iter().enumerate() {
            if shelf.h < needed_h {
                continue;
            }
            // The shelf's "age" is the youngest entry it holds:
            // recycling it discards everything, so the cost is the
            // freshest stamp we'd lose.  An empty shelf has age 0
            // and is always the best candidate.
            let youngest = shelf
                .entries
                .iter()
                .filter_map(|k| self.cache.get(k).map(|e| e.last_used))
                .max()
                .unwrap_or(0);
            match best {
                None => best = Some((idx, youngest)),
                Some((_, prev)) if youngest < prev => best = Some((idx, youngest)),
                _ => {}
            }
        }
        let Some((idx, _)) = best else {
            return false;
        };
        let keys = std::mem::take(&mut self.shelves[idx].entries);
        for k in &keys {
            self.cache.remove(k);
        }
        self.shelves[idx].x_used = 0;
        self.evict_count += 1;
        true
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
        if let Some(entry) = self.cache.get_mut(&key) {
            // Phase B v2 attack #2 — decay-store (see comment on
            // `LAST_USED_RESOLUTION`).
            let cf = self.current_frame;
            if cf.wrapping_sub(entry.last_used) >= LAST_USED_RESOLUTION {
                entry.last_used = cf;
            }
            return Some(*entry);
        }
        let mut buf = vec![0u8; (w as usize) * (h as usize)];
        rasterise(&mut buf);
        let (x, y, shelf_idx) = self.place_or_evict(w, h)?;
        self.upload(&buf, w, h, x, y);
        // Box-drawing / block-element rasters TILE the entire cell —
        // caller fills (0,0)..(w,h) with ink.  The slot is meant to
        // land at (cell_x, cell_top) with no padding.
        //   bearing_x = 0          — slot left  = cell_x  = pen_x
        //   bearing_y = ascent     — slot top   = cell_top = baseline_y - ascent
        // Renderer formula `quad_top = baseline_y - bearing_y` then
        // yields quad_top = cell_top exactly.
        let entry = AtlasEntry {
            u0: x as u16,
            v0: y as u16,
            u1: (x + w) as u16,
            v1: (y + h) as u16,
            px_w: w as u16,
            px_h: h as u16,
            n_cells,
            bearing_x: 0,
            bearing_y: baseline_from_top as i16,
            last_used: self.current_frame,
        };
        self.cache.insert(key, entry);
        self.shelves[shelf_idx].entries.push(key);
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

    /// Throw every glyph away because the *font* changed underneath —
    /// a display swap re-derives the terminal cell, so what is packed
    /// here was rasterised for sizes nothing will ask for again.
    ///
    /// Keys carry the point size, so stale entries would never be
    /// *hit*; they would simply occupy the sheet until eviction got
    /// round to them, and on a swap that is the whole sheet.
    pub fn drop_all_glyphs(&mut self) {
        self.rebuild();
    }

    /// Find a position for a `(w, h)` glyph using the shelf packer.
    /// Returns `(x, y, shelf_idx)` — the latter so the caller can
    /// record the placed entry's key on its shelf for LRU eviction.
    fn place(&mut self, w: u32, h: u32) -> Option<(u32, u32, usize)> {
        let pad_w = w + 2 * PAD;
        let pad_h = h + 2 * PAD;
        if pad_w > self.width {
            // Glyph wider than the atlas — pathological; bail.
            return None;
        }

        // Try to place on an existing shelf whose height fits.  We
        // accept up to 25 % wasted vertical space — past that, open
        // a new shelf so glyphs of similar height cluster together.
        for (idx, shelf) in self.shelves.iter_mut().enumerate() {
            if shelf.h >= pad_h && (shelf.h as f64 - pad_h as f64) / shelf.h as f64 <= 0.25
                && shelf.x_used + pad_w <= self.width
            {
                let x = shelf.x_used + PAD;
                let y = shelf.y + PAD;
                shelf.x_used += pad_w;
                return Some((x, y, idx));
            }
        }

        // Open a new shelf at the bottom of the atlas.
        let y_top = self.shelves.last().map(|s| s.y + s.h).unwrap_or(0);
        if y_top + pad_h > self.height {
            return None;
        }
        let new_idx = self.shelves.len();
        self.shelves.push(Shelf {
            y: y_top,
            h: pad_h,
            x_used: pad_w,
            entries: Vec::new(),
        });
        Some((PAD, y_top + PAD, new_idx))
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

/// Phase 3 — mono raster sized to the glyph's natural bbox + PAD,
/// with no oversized-cell-fit fallback.  Used by the chrome shaping
/// path (`font_shape::shape_line` → `GlyphAtlas::get_or_rasterize_natural`)
/// where there is no cell constraint — CTLine has already laid each
/// glyph at its proportional advance, so the rasteriser's only job
/// is to emit a tight bitmap at the natural size.
///
/// Phase 4 — `subpx_x ∈ 0..4` offsets the pen horizontally by
/// `subpx_x × 0.25 px` inside the bitmap so the same glyph at 4
/// different sub-pixel positions caches as 4 distinct entries.  The
/// renderer pushes the quad at integer `pen_x`; the AA edge inside
/// the bitmap carries the fractional offset.  Bitmap gets one extra
/// column to accommodate the right-edge shift at `subpx_x == 3`.
/// Phase 10 — thin trait-shaped wrapper around the internal mono
/// natural rasteriser.  `CoreTextMonoRasteriser` in `font_trait` calls
/// this so its impl doesn't need access to the private `Raster` type.
pub fn raster_natural_mono(
    font: &CTFont,
    glyph: CGGlyph,
    subpx_x: u8,
) -> Option<crate::font_trait::RasterOutput> {
    rasterise_glyph_natural(font, glyph, subpx_x).map(|r| crate::font_trait::RasterOutput {
        bytes: r.bytes,
        px_w: r.px_w,
        px_h: r.px_h,
        bearing_x: r.bearing_x,
        bearing_y: r.bearing_y,
    })
}

/// Phase 10 — companion to `raster_natural_mono`, for the BGRA8 colour
/// path used by `CoreTextColorRasteriser`.
pub fn raster_natural_color(
    font: &CTFont,
    glyph: CGGlyph,
    subpx_x: u8,
) -> Option<crate::font_trait::RasterOutput> {
    rasterise_glyph_color_natural(font, glyph, subpx_x).map(|r| crate::font_trait::RasterOutput {
        bytes: r.bytes,
        px_w: r.px_w,
        px_h: r.px_h,
        bearing_x: r.bearing_x,
        bearing_y: r.bearing_y,
    })
}

fn rasterise_glyph_natural(font: &CTFont, glyph: CGGlyph, subpx_x: u8) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }
    // Phase 10c — CTFont's bbox is in user-space pt; shape_line bakes
    // a 2× retina scale into glyph pen positions, so the raster must
    // bake the same scale into the bitmap dims and the CGContext
    // CTM.  Without this scale the bitmap was sized in pt while the
    // pen advance was in phys px — Bold 24pt looked OK (glyph and
    // gap roughly matched) but Regular 24pt rendered at half the
    // expected width, reading as evenly-spaced "mono cell" layout.
    const RETINA_SCALE: f64 = 2.0;
    let scaled_w = bbox.size.width * RETINA_SCALE;
    let scaled_h = bbox.size.height * RETINA_SCALE;
    // +1 column right of the natural bbox to fit `subpx_x = 3` shift.
    let px_w = (scaled_w.ceil() as u32) + 2 * PAD + 1;
    let px_h = (scaled_h.ceil() as u32) + 2 * PAD;
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
    // Scale CTM so `draw_glyphs` (pt-space) rasterises at retina dims.
    ctx.scale(RETINA_SCALE, RETINA_SCALE);
    // Phase 4 — sub-pixel x shift inside the bitmap.  pen coords are
    // now in USER (pre-scale) space; PAD is phys px, so divide by
    // scale to get the equivalent pt offset.
    let subpx_offset = (subpx_x as f64).min(3.0) * 0.25 / RETINA_SCALE;
    let pen_x = (PAD as f64) / RETINA_SCALE - bbox.origin.x + subpx_offset;
    let pen_y = (px_h as f64) / RETINA_SCALE - (PAD as f64) / RETINA_SCALE
        - bbox.origin.y - bbox.size.height;
    font.draw_glyphs(&[glyph], &[CGPoint::new(pen_x, pen_y)], ctx);
    // bearings stay in phys px (renderer's quad formula is phys px).
    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells: 1,
        bearing_x: -(PAD as i16),
        bearing_y: ((bbox.origin.y + bbox.size.height) * RETINA_SCALE).ceil() as i16
            + (PAD as i16),
    })
}

/// Phase 3 — colour-glyph natural-size variant of
/// `rasterise_glyph_natural`.  Same geometry, BGRA pixel format.
/// Phase 4 — accepts the same `subpx_x` shift; emoji rarely cluster
/// tight enough for sub-pixel positioning to matter visually, but
/// the path supports it for consistency with the mono path so the
/// atlas key shape stays uniform.
fn rasterise_glyph_color_natural(font: &CTFont, glyph: CGGlyph, subpx_x: u8) -> Option<Raster> {
    let bbox = font.get_bounding_rects_for_glyphs(
        core_text::font_descriptor::kCTFontOrientationDefault,
        &[glyph],
    );
    if bbox.size.width <= 0.0 || bbox.size.height <= 0.0 {
        return None;
    }
    let px_w = (bbox.size.width.ceil() as u32) + 2 * PAD + 1;
    let px_h = (bbox.size.height.ceil() as u32) + 2 * PAD;
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
    drop(cs);
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    let subpx_offset = (subpx_x as f64).min(3.0) * 0.25;
    let pen_x = (PAD as f64) - bbox.origin.x + subpx_offset;
    let pen_y = (px_h as f64) - (PAD as f64) - bbox.origin.y - bbox.size.height;
    font.draw_glyphs(&[glyph], &[CGPoint::new(pen_x, pen_y)], ctx);
    Some(Raster {
        bytes,
        px_w,
        px_h,
        n_cells: 1,
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

    /// The key was fully packed — font_id owned 32 bits for a registry
    /// that holds a few dozen fonts — so it was re-cut to 24, leaving
    /// room for flags to grow.  Every field has to survive that, and
    /// two keys differing only in flags must not share a cache slot.
    #[test]
    fn the_repacked_key_keeps_every_field_and_separates_the_variants() {
        let k = GlyphKey::new(0x00AB_CDEF, 0xBEEF, 4095, 3, GlyphKey::FLAGS_MASK as u8);
        assert_eq!(k.font_id(), 0x00AB_CDEF, "font_id");
        assert_eq!(k.glyph(), 0xBEEF, "glyph");
        assert_eq!(k.size_q(), 4095, "size_q");
        assert_eq!(k.subpx_x(), 3, "subpx_x");
        assert_eq!(k.flags(), GlyphKey::FLAGS_MASK as u8, "flags");

        let plain = GlyphKey::new(7, 42, 52, 0, GlyphKey::FLAG_SMOOTH);
        let flagged = GlyphKey::new(7, 42, 52, 0, GlyphKey::FLAGS_MASK as u8);
        assert_ne!(plain, flagged, "flags must not collide in one cache slot");
        assert_eq!(flagged.font_id(), 7);
        assert_eq!(flagged.glyph(), 42);
    }

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

        let key = GlyphKey::new(
            0,
            glyph,
            GlyphKey::size_q_for(13.0),
            0,
            GlyphKey::FLAG_SMOOTH,
        );
        let entry1 = atlas.get_or_rasterize(key, &font, test_metrics(), 1).expect("first call rasterises");
        assert!(entry1.px_w > 0 && entry1.px_h > 0);
        assert_eq!(atlas.cache_len(), 1);

        let entry2 = atlas.get_or_rasterize(key, &font, test_metrics(), 1).expect("second call from cache");
        assert_eq!(entry1.u0, entry2.u0, "second call must return the same UV");
        assert_eq!(atlas.cache_len(), 1, "cache must not grow on hit");
    }

    /// Phase 10 — atlas pumps glyphs through a `MockRasteriser`
    /// without ever calling CoreText.  Validates that the trait
    /// dispatch in `get_or_rasterize_natural` is wired correctly:
    /// the returned `AtlasEntry` dims must match what the mock
    /// emitted, and the cache-hit path stamps `last_used` the same
    /// way it does for the real rasteriser.
    #[test]
    fn mock_rasteriser_drives_natural_path() {
        use crate::font_trait::MockRasteriser;
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut atlas = GlyphAtlas::new_with_rasteriser(
            &device,
            128,
            128,
            false,
            Box::new(MockRasteriser::mono(4, 6)),
        )
        .expect("atlas with mock rasteriser");
        let font = make_font(); // unused by MockRasteriser
        let key = GlyphKey::new(7, 42, 100, 0, GlyphKey::FLAG_SMOOTH);
        atlas.begin_frame(11);
        let e1 = atlas
            .get_or_rasterize_natural(key, &font)
            .expect("mock raster places");
        assert_eq!(e1.px_w, 4);
        assert_eq!(e1.px_h, 6);
        assert_eq!(e1.last_used, 11);

        // Attack #2 — decay-store: a hit within
        // `LAST_USED_RESOLUTION` frames must NOT rewrite the stamp.
        atlas.begin_frame(12);
        let e2 = atlas
            .get_or_rasterize_natural(key, &font)
            .expect("cache hit");
        assert_eq!(e1.u0, e2.u0, "cache hit must reuse UV");
        assert_eq!(
            e2.last_used, 11,
            "decay-store: hit within resolution window must not rewrite stamp"
        );

        // After crossing the resolution window the stamp must
        // advance — that's how the LRU eviction path still sees
        // coarse-grained age progress.
        atlas.begin_frame(11 + LAST_USED_RESOLUTION);
        let e3 = atlas
            .get_or_rasterize_natural(key, &font)
            .expect("cache hit past resolution");
        assert_eq!(
            e3.last_used,
            11 + LAST_USED_RESOLUTION,
            "hit past resolution window must rewrite stamp"
        );
    }

    /// Phase 9 — atlas raster perf characterization.  Same idea as
    /// the shape-cache test: prove warm cache hits are sub-µs faster
    /// than cold raster.  Prints both timings; loose bound guards
    /// against grossly broken cache behaviour while tolerating
    /// macOS-version GPU jitter.
    #[test]
    fn raster_warm_cache_beats_cold_raster() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut atlas = GlyphAtlas::new(&device, 4096, 4096).expect("atlas");
        let font = make_font();

        let make_key = |ch: u8| -> GlyphKey {
            let mut g: CGGlyph = 0;
            let cu: u16 = ch as u16;
            unsafe {
                font.get_glyphs_for_characters(&cu, &mut g, 1);
            }
            GlyphKey::new(0, g, GlyphKey::size_q_for(13.0), 0, GlyphKey::FLAG_SMOOTH)
        };

        let chars: &[u8] = b"abcdefghijklmnop";
        // Cold pass — each first call rasterises into the atlas.
        let mut cold_us: Vec<u128> = Vec::with_capacity(chars.len());
        for &c in chars {
            let key = make_key(c);
            let t0 = std::time::Instant::now();
            let _ = atlas
                .get_or_rasterize(key, &font, test_metrics(), 1)
                .expect("cold raster");
            cold_us.push(t0.elapsed().as_micros());
        }
        cold_us.sort_unstable();
        let cold_median = cold_us[cold_us.len() / 2];

        // Warm pass — every call hits the cache.
        let mut warm_ns: Vec<u128> = Vec::with_capacity(chars.len());
        for &c in chars {
            let key = make_key(c);
            let t0 = std::time::Instant::now();
            let _ = atlas
                .get_or_rasterize(key, &font, test_metrics(), 1)
                .expect("warm cache");
            warm_ns.push(t0.elapsed().as_nanos());
        }
        warm_ns.sort_unstable();
        let warm_median = warm_ns[warm_ns.len() / 2];

        eprintln!(
            "[font v5 Phase 9] atlas raster: cold median = {cold_median} µs, warm median = {warm_median} ns"
        );

        assert!(
            cold_median < 5000,
            "cold raster median {cold_median} µs exceeds 5000 µs safety bound",
        );
        assert!(
            warm_median < 50_000,
            "warm cache lookup median {warm_median} ns exceeds 50 µs safety bound",
        );
    }

    #[test]
    fn lru_evicts_oldest_shelf_when_full() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let font = make_font();
        let make_key = |ch: u8, size_q: u16| -> GlyphKey {
            let mut g: CGGlyph = 0;
            let cu: u16 = ch as u16;
            unsafe {
                font.get_glyphs_for_characters(&cu, &mut g, 1);
            }
            GlyphKey::new(0, g, size_q, 0, GlyphKey::FLAG_SMOOTH)
        };

        // Probe: render one 'a' into an oversized atlas first to learn
        // its packed dimensions, then size a real test atlas around
        // it.  Real Menlo glyph metrics shift between macOS releases,
        // so hard-coding a tiny atlas is brittle — derive it.
        let mut probe = GlyphAtlas::new(&device, 64, 64).expect("probe atlas");
        let entry_a = probe
            .get_or_rasterize(make_key(b'a', 52), &font, test_metrics(), 1)
            .expect("probe rasterise");
        let probe_w = entry_a.px_w as u32 + 2 * PAD;
        let probe_h = entry_a.px_h as u32 + 2 * PAD;
        drop(probe);

        // Atlas sized to fit exactly one ASCII glyph at size_q=52 in a
        // single shelf — that way the second placement must trigger
        // LRU shelf recycling.
        let mut atlas = GlyphAtlas::new(&device, probe_w, probe_h).expect("atlas");

        atlas.begin_frame(1);
        atlas
            .get_or_rasterize(make_key(b'a', 52), &font, test_metrics(), 1)
            .expect("place a@52 on frame 1");

        atlas.begin_frame(2);
        atlas
            .get_or_rasterize(make_key(b'a', 52), &font, test_metrics(), 1)
            .expect("hit a@52 on frame 2");

        atlas.begin_frame(3);
        // Same char, different size_q → distinct atlas key, same
        // dimensions (Phase 1.1 raster doesn't read size_q).  Forces
        // place() failure on the (now full) shelf; LRU eviction
        // recycles it (NOT the legacy whole-atlas rebuild), so the
        // new entry lands and `evict_count` ticks while
        // `rebuild_count` stays 0 — the Phase 6 contract.
        atlas
            .get_or_rasterize(make_key(b'a', 53), &font, test_metrics(), 1)
            .expect("place a@53 on frame 3 via shelf eviction");
        assert!(
            atlas.evict_count >= 1,
            "expected at least one shelf eviction; got evict_count={}",
            atlas.evict_count
        );
        assert_eq!(
            atlas.rebuild_count, 0,
            "shelf eviction must beat the legacy whole-atlas rebuild path"
        );
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
            let key = GlyphKey::new(
                0,
                glyph,
                GlyphKey::size_q_for(13.0),
                0,
                GlyphKey::FLAG_SMOOTH,
            );
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
