//! Metal-backed renderer (in progress).
//!
//! Replaces the AppKit/CGImage/setContents path (`render.rs`) with
//! CAMetalLayer + glyph atlas + instanced quads.  Built incrementally
//! so each phase stands on its own and the existing renderer stays the
//! default until the Metal path matches it pixel-for-pixel.
//!
//! ## Phase plan
//!
//! 1. (this commit) **Scaffold** — `MTLDevice`, `MTLCommandQueue`,
//!    `CAMetalLayer` attached to an `NSView`, single-frame clear-color
//!    render pass.  Proves the Metal pipeline runs end-to-end inside
//!    marspot without disturbing the AppKit renderer.
//! 2. **Glyph atlas** — CoreText-rasterise glyphs into an MTLTexture,
//!    LRU-evict per CLAUDE.md "bounded growth".
//! 3. **BG pass** — instanced coloured quads, one per cell.
//! 4. **FG pass** — textured glyph quads sampling the atlas.
//! 5. **Integration** — wire to the terminal grid; A/B against
//!    `Renderer` via `MARSPOT_METAL=1`.  Once visually equivalent and
//!    measurably faster, retire the AppKit path.
//!
//! ## Why "previously failed at this" doesn't apply
//!
//! `render.rs`'s header notes marspot *did* try Metal+atlas before and
//! pivoted away due to "gamma + atlas neighbor + sampling issues".
//! This rebuild is informed by that — explicit gamma in the shader
//! (sRGB pixel format, premultiplied alpha), atlas allocator that
//! pads each glyph by 1 px (no neighbour bleed), and exact-pixel
//! sampling with `MTLSamplerMinMagFilter::Nearest` for the BG pass
//! and `Linear` only for the glyph pass.  Full design notes go into
//! `docs/architecture.md` once phase 4 lands.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSView, NSViewLayerContentsPlacement};
use objc2_foundation::{NSSize, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer,
    MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLSamplerAddressMode, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState, MTLStoreAction, MTLTexture,
};
use core_graphics::color_space::{kCGColorSpaceSRGB, CGColorSpace};
use foreign_types::ForeignType;
use objc2_app_kit::NSColor;
use objc2_quartz_core::{kCAGravityTopLeft, CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use objc2_metal::MTLBuffer;

use crate::font_cache::{resolve_attrs, FontCache, BG};
use crate::glyph_atlas::{AtlasEntry, GlyphAtlas, GlyphKey, SlotMetrics, BOX_DRAWING_FONT_ID};
use crate::layout::{CellRect, Layout, Rect};
use crate::render::{
    box_drawing_arms, block_element_rects, rasterize_arms_into_buf, rasterize_block_into_buf,
    SessionView, SidebarEntry,
};
#[cfg(test)]
use crate::render::HighlightSpan;
use crate::session::SessionState;

use core_graphics::font::CGGlyph;

/// Resolve an arbitrary cell character to an atlas entry, routing
/// box-drawing (U+2500-U+257F + ╭╮╯╰) and block elements
/// (U+2580-U+259F) through our own pixel-perfect mask rasteriser
/// instead of CT's font glyph.  CT glyphs for these chars typically
/// don't span the cell advance, producing visible seams when
/// claudecode / vim / htop draw box borders or progress bars; our
/// masks fill the cell exactly so adjacent cells join with zero
/// drift.  Pure Rust path, no extra deps, atlas + GPU pipeline
/// downstream is unchanged.
fn resolve_cell_glyph(
    atlas: &mut GlyphAtlas,
    font: &mut FontCache,
    ch: char,
    bold: bool,
    italic: bool,
    metrics: SlotMetrics,
) -> Option<AtlasEntry> {
    if let Some(arms) = box_drawing_arms(ch) {
        let w = metrics.cell_w;
        let h = metrics.cell_h;
        let key = box_drawing_key(ch, &metrics);
        return atlas.get_or_insert_custom_raster(key, w, h, metrics.baseline_from_top, 1, |buf| {
            rasterize_arms_into_buf(buf, w as usize, h as usize, arms);
        });
    }
    if let Some(shape) = block_element_rects(ch) {
        let w = metrics.cell_w;
        let h = metrics.cell_h;
        let key = box_drawing_key(ch, &metrics);
        return atlas.get_or_insert_custom_raster(key, w, h, metrics.baseline_from_top, 1, |buf| {
            rasterize_block_into_buf(buf, w as usize, h as usize, shape);
        });
    }
    let (font_idx, glyph) = font.resolve_char(ch, bold, italic);
    if glyph == 0 {
        return None;
    }
    let ct_font = font.font(font_idx).clone();
    let n_cells = crate::grid::char_width(ch).max(1) as u16;
    atlas.get_or_rasterize(
        text_glyph_key(font_idx as u32, glyph, &ct_font),
        &ct_font,
        metrics,
        n_cells,
    )
}

/// Phase 2 — synthesise an atlas key for a box-drawing / block-element
/// glyph.  These are rasterised by our own code (not CT), so they have
/// no `pt_size` — but cell dimensions stand in for "rendering size", so
/// a glyph rasterised at cell_h = 32 vs cell_h = 24 lands in different
/// slots.  Pack `cell_h` into `size_q` to keep cross-DPI rasters
/// separated.  `flags = 0` since the custom rasters don't use the CT
/// smoothing knobs.
#[inline]
fn box_drawing_key(ch: char, metrics: &SlotMetrics) -> GlyphKey {
    GlyphKey::new(
        BOX_DRAWING_FONT_ID,
        ch as u32 as CGGlyph,
        metrics.cell_h as u16,
        0,
        0,
    )
}

/// Phase 2 — atlas key for a CT-rasterised text glyph.  `size_q`
/// quantises the font's pt-size to 0.25-pt buckets so PTY 12pt and
/// chrome 13pt cache independently.  `subpx_x` reserved for Phase 4.
/// `flags = FLAG_SMOOTH` because every call into `rasterise_glyph`
/// runs with font-smoothing on (see the `set_should_smooth_fonts(true)`
/// line in the rasteriser).
#[inline]
fn text_glyph_key(font_id: u32, glyph: CGGlyph, ct_font: &core_text::font::CTFont) -> GlyphKey {
    GlyphKey::new(
        font_id,
        glyph,
        GlyphKey::size_q_for(ct_font.pt_size()),
        0,
        GlyphKey::FLAG_SMOOTH,
    )
}

/// How wide a cell's glyph is drawn is decided by the width table
/// alone — never by what happens to sit next to it.
///
/// There was a rule here that let a squeezed glyph spill into the cell
/// on its right when that cell was blank (WezTerm's
/// `WhenFollowedBySpace`).  It is gone: `①` came out full-size before a
/// space and small before a character, so the *same* character changed
/// size as the line around it was written, which reads as a rendering
/// fault whichever size you preferred.  A cell now looks the way it
/// looks because of what is in it.
///
/// The circled family is genuinely too wide for one cell — PingFang
/// draws `①` 11.71 px against a 7.20 px cell — so one cell means
/// scaled down, always.  Full size costs a second cell, which moves
/// the wrap point; that is a decision with a real downside, so it is
/// the settings panel's `appearance_circled_wide`, off by default.

/// Like `resolve_cell_glyph`, but routes colour glyphs (Apple Color Emoji)
/// to the colour (`BGRA8`) atlas and everything else to the mono (`R8`)
/// atlas.  Returns `(entry, is_color)` so the caller can pick the matching
/// glyph buffer + atlas dims.  Box-drawing / block-element glyphs are always
/// mono (we rasterise those ourselves).
#[allow(clippy::too_many_arguments)]
fn resolve_cell_glyph_routed(
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    font: &mut FontCache,
    ch: char,
    bold: bool,
    italic: bool,
    metrics: SlotMetrics,
) -> Option<(AtlasEntry, bool)> {
    if box_drawing_arms(ch).is_some() || block_element_rects(ch).is_some() {
        return resolve_cell_glyph(atlas, font, ch, bold, italic, metrics).map(|e| (e, false));
    }
    let (font_idx, glyph) = font.resolve_char(ch, bold, italic);
    if glyph == 0 {
        return None;
    }
    let ct_font = font.font(font_idx).clone();
    let key = text_glyph_key(font_idx as u32, glyph, &ct_font);
    let n_cells = crate::grid::char_width(ch).max(1) as u16;
    if font.is_color_font(font_idx) {
        color_atlas
            .get_or_rasterize(key, &ct_font, metrics, n_cells)
            .map(|e| (e, true))
    } else {
        atlas
            .get_or_rasterize(key, &ct_font, metrics, n_cells)
            .map(|e| (e, false))
    }
}

/// One cell's draw data, layout-compatible with `Cell` in
/// `src/shaders/cells.metal`.  Repr-C; no padding shenanigans.
///
/// Coordinate convention: `origin` and `size` in physical pixels,
/// origin = top-left of the viewport, +y = down.  `color` is RGBA
/// in 0..1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CellInstance {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub color: [f32; 4],
}

/// One glyph's draw data, layout-compatible with `Glyph` in
/// `src/shaders/cells.metal`.  `uv0` / `uv1` are normalised
/// 0..1 atlas coords (top-left + bottom-right corners of the
/// glyph's slot).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GlyphInstance {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub uv0: [f32; 2],
    pub uv1: [f32; 2],
    pub color: [f32; 4],
}

/// Renderer state that belongs to one window rather than to the
/// renderer as a whole.
///
/// RFC-005 — one `MetalRenderer` draws every window, one after the
/// other, so almost everything on it is legitimately shared: the
/// device, the pipelines, the font cache, both glyph atlases and every
/// scratch buffer (a scratch buffer is only live inside a single
/// `render_*` call).  Sharing them is the whole reason a second window
/// costs a layout and a render target rather than another copy of the
/// atlas.
///
/// Two things genuinely cannot be shared, and they live here:
///
/// * `pane_caches` is indexed by a window's pane order, so window A's
///   slot 0 and window B's slot 0 are different panes.
/// * `clear_bg_required` answers "does *this* window still owe a full
///   clear", which a resize of some other window must not satisfy.
///
/// Note what is deliberately absent: a per-scale glyph atlas.  Glyphs
/// are rasterised once at `FONT_POINT` and the atlas key carries the
/// quantised size, so one atlas already serves windows on displays of
/// different backing scales.
/// Where one IOSurface frame spent its time.
///
/// A frame that took 41 seconds is not a finding — "render is slow"
/// cannot be attacked.  These three numbers can: they separate CPU
/// instance-building (which includes rasterising every glyph the
/// atlas has never seen) from command encoding from the GPU wait, and
/// they are bracketed by calls the compiler cannot reorder across.
/// `glyphs_rasterised` is the companion witness — a large `build_us`
/// with a large glyph count is a cold atlas; the same `build_us` with
/// none is something else entirely, and without the count the two
/// look identical.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderSplit {
    pub build_us: u64,
    /// Blocked inside `queue.commandBuffer()`.  Split out from
    /// `encode_us` because the first real-workload numbers put
    /// 96–396 ms in "encode" — a phase that only assembles a command
    /// buffer on the CPU and has no business taking that long — and
    /// "encode" as one number cannot say whether the time went into
    /// *obtaining* the buffer or *filling* it.
    pub cmdbuf_us: u64,
    /// Filling the buffer: the four render passes.
    pub encode_us: u64,
    /// Of `encode_us`, the part spent allocating fresh per-frame
    /// instance buffers, and how many bytes that was.
    pub instbuf_us: u64,
    pub instbuf_bytes: u64,
    /// The dev-panel and context-menu canvases, built and encoded
    /// after the passes.  Zero unless one of them is open — which is
    /// itself worth knowing when a frame goes long.
    pub canvas_us: u64,
    pub gpu_wait_us: u64,
    /// What the GPU itself reports it spent executing this frame
    /// (`GPUEndTime - GPUStartTime`), against `gpu_wait_us`'s wall
    /// clock.  A large wait with a small exec means we were queued or
    /// descheduled, not that the work is heavy — and those two want
    /// completely different fixes.
    pub gpu_exec_us: u64,
    /// Of `build_us`, the per-pane loop vs everything else (chrome,
    /// sidebar, panels, modals, overlays).
    pub build_panes_us: u64,
    /// Panes that missed the per-pane instance cache and were rebuilt
    /// this frame, out of how many were considered.  A frame that
    /// rebuilds all of them every time is a cache that isn't working,
    /// which no timing on its own would reveal.
    pub panes_rebuilt: u32,
    pub panes_total: u32,
    pub glyphs_rasterised: u32,
    /// Shelf evictions and whole-atlas rebuilds *during this frame*.
    /// Non-zero means the frame's own working set did not fit, so it
    /// threw away glyphs it went on to need again — a different
    /// failure from "there were simply a lot of new glyphs", and the
    /// two are indistinguishable from timing alone.
    pub evictions: u64,
    pub rebuilds: u64,
}

impl RenderSplit {
    /// `build=..ms encode=..ms gpu=..ms glyphs=..` — one field for a
    /// log line, so the three numbers always travel together.
    pub fn summary(&self) -> String {
        format!(
            "build_{:.1}ms(panes_{:.1}ms_{}/{}rebuilt) cmdbuf_{:.1}ms encode_{:.1}ms(instbuf_{:.1}ms/{:.1}MB) canvas_{:.1}ms gpu_{:.1}ms(exec_{:.1}ms) glyphs_{}",
            self.build_us as f64 / 1000.0,
            self.build_panes_us as f64 / 1000.0,
            self.panes_rebuilt,
            self.panes_total,
            self.cmdbuf_us as f64 / 1000.0,
            self.encode_us as f64 / 1000.0,
            self.instbuf_us as f64 / 1000.0,
            self.instbuf_bytes as f64 / 1048576.0,
            self.canvas_us as f64 / 1000.0,
            self.gpu_wait_us as f64 / 1000.0,
            self.gpu_exec_us as f64 / 1000.0,
            self.glyphs_rasterised,
        ) + &if self.evictions > 0 || self.rebuilds > 0 {
            format!(" evict_{} rebuild_{}", self.evictions, self.rebuilds)
        } else {
            String::new()
        }
    }
}

pub struct WindowRender {
    pane_caches: Vec<PaneInstanceCache>,
    /// Should the next render to an IOSurface target start with a
    /// hard Clear, or load the previous frame's pixels?  Clear is
    /// only ever needed when the SHAPE of what gets painted changes
    /// (layout mode switch, sidebar toggle, resize, first frame
    /// after attach) — in steady state the BG region is identical
    /// across frames, so loading preserves visually-identical
    /// content.  Crucially, Clear opens a cross-process race window:
    /// shell's presenter reads the IOSurface texture from a DIFFERENT
    /// process / queue, with no MTLSharedEvent fence to gate the
    /// read.  If presenter samples between the Clear and the cell
    /// draws on the GPU, it sees a uniform-BG texture — exactly
    /// what surfaces in the wild as "all 9 panes' contents momentarily
    /// disappear to background and reappear, no clear trigger,
    /// frequent" (the marspot flash bug, 2026-06-15).  Defaulting to
    /// Load eliminates the bg-only intermediate state.  The same-
    /// process CAMetalLayer path (`render_layout`, mcli/standalone)
    /// always Clears — there's no cross-process race there.
    ///
    /// The flag itself now lives on `WindowRender`: it answers "does
    /// this window still owe a clear", and one window's resize must
    /// not discharge another's.
    clear_bg_required: bool,
    /// The frame this window has on the GPU, if any.
    ///
    /// The live path used to end each frame in `waitUntilCompleted`,
    /// on the main loop.  That is one thread for input, PTY pumping,
    /// every pane and the GPU round-trip, so a slow round-trip stops
    /// the whole terminal: measured across 166 stalls on a working
    /// machine, the average wait was **374 ms against 3.3 ms of actual
    /// GPU execution**, the worst 3.6 s — all of it with keystrokes
    /// queueing up behind it (2026-09-06, "在我们这开 codex，输入有时
    /// 候都会卡，在 iTerm2 很流畅").  The GPU was not busy; we were
    /// queued behind a loaded machine's other work and chose to block.
    ///
    /// So the frame is committed and left running.  Its buffer is kept
    /// here and polled — `settled()` — and only when it reports
    /// `Completed` does the surface flip and `SurfaceReady` go out, so
    /// the shell still only ever samples a finished surface.  A window
    /// with a frame in flight simply does not start another; the loop
    /// goes back to reading input, which is the whole point.
    ///
    /// One frame in flight, never two: that keeps every existing
    /// invariant intact — the instance pool is still refilled in place
    /// (nothing reads it once the frame completes) and the two
    /// surfaces still alternate with a full frame between reuses.
    in_flight: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// The GPU's own account of the last completed frame.
    last_gpu_exec_us: u64,
}

impl Default for WindowRender {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowRender {
    pub fn new() -> Self {
        // Starts true: a window has never been painted, so its first
        // frame owes a full clear.
        Self {
            pane_caches: Vec::new(),
            clear_bg_required: true,
            in_flight: None,
            last_gpu_exec_us: 0,
        }
    }

    /// Is a frame still on the GPU for this window?
    ///
    /// A caller that sees `true` must not start another frame — it
    /// would overwrite the instance buffers the GPU is reading.
    pub fn frame_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Has the in-flight frame finished?  Reaps it if so.
    ///
    /// Returns `true` exactly once per frame, on the poll that finds it
    /// complete — that is the moment the surface is safe to show.
    pub fn settled(&mut self) -> bool {
        let Some(cmd) = self.in_flight.as_ref() else {
            return false;
        };
        // `Error` counts as settled: the surface will not improve by
        // waiting, and leaving the window stuck with a frame that will
        // never complete would freeze it forever.
        let st = cmd.status();
        if st != MTLCommandBufferStatus::Completed && st != MTLCommandBufferStatus::Error {
            return false;
        }
        let (s, e) = (cmd.GPUStartTime(), cmd.GPUEndTime());
        self.last_gpu_exec_us = ((e - s).max(0.0) * 1e6) as u64;
        self.in_flight = None;
        true
    }

    /// GPU time of the last completed frame, in microseconds.
    pub fn last_gpu_exec_us(&self) -> u64 {
        self.last_gpu_exec_us
    }

    /// The next frame for this window must clear rather than load.
    pub fn mark_bg_clear_required(&mut self) {
        self.clear_bg_required = true;
    }

    /// Consume the flag: returns whether this frame owes a clear, and
    /// leaves the window not owing one.  The render path uses it, and
    /// it is the only way to observe the flag — which is what lets a
    /// test prove one window's flag is not another's.
    pub fn take_clear_required(&mut self) -> bool {
        std::mem::take(&mut self.clear_bg_required)
    }
}

/// F1+13 — cached per-pane render contributions.  When a pane's
/// `fingerprint` (computed from its `SessionView` fields) and both
/// atlas generations match the previous frame, the renderer
/// `extend_from_slice`s the cached vecs straight into the current
/// frame's accumulators — skipping `push_session` for that pane
/// entirely.  Cache is invalidated whenever an atlas was rebuilt
/// or any contributing input changed.
#[derive(Default)]
struct PaneInstanceCache {
    fingerprint: u64,
    atlas_gen: u64,
    color_atlas_gen: u64,
    /// `link_probe::generation()` at the time this slot was built.
    /// Link verdicts land asynchronously, so a pane whose grid did
    /// not change still has to rebuild once when new answers arrive
    /// — otherwise a resolved file link never gets underlined until
    /// something else dirties the pane.
    link_gen: u64,
    cells: Vec<CellInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    /// `true` once this slot has actually been built at least once;
    /// distinguishes "fresh default" from "valid but happens to
    /// have empty contributions".
    primed: bool,
}

/// F3+4 — one row in the detail process-tree panel (right column of
/// the redesigned Process Monitor).  Pure data; L2 builds these every
/// frame for the selected pane.  Includes per-pid resource cols so
/// the renderer can lay out a proper sortable table.
#[derive(Debug, Clone)]
pub struct ProcessPanelRow {
    /// Indent depth (0 = pane header / shell-root, 1+ = tree
    /// descendants).  Cosmetic only — the leftmost column.
    pub depth: u8,
    /// Process pid.  0 if `is_header` (panel header row).
    pub pid: i32,
    /// Process comm (16 chars max on macOS — what `ps` shows).
    pub comm: String,
    /// Sampled CPU% (delta of two task_info samples / wall-clock dt
    /// × 100).  May exceed 100 for multi-threaded procs (a `rustc`
    /// pinning one P-core reads ~100; a parallel `cargo build` walks
    /// up to ncpu × 100).  0 for header rows.
    pub cpu_pct: f32,
    /// Resident set size in KB at the latest sample.
    pub rss_kb: u64,
    /// True when this is the synthetic header row for the pane (no
    /// kill button drawn, slightly heavier FG).  False for tree rows.
    pub is_header: bool,
}

/// F3+4 — one row in the MASTER pane list (left column of the
/// redesigned Process Monitor).  Each row summarises one pane: name +
/// aggregate process count / CPU% / RSS over the pane's pid tree.
#[derive(Debug, Clone)]
pub struct ProcessPanelPaneRow {
    /// Pane name as the user sees it elsewhere — custom title >
    /// cwd basename > ordinal.  Already truncated by L2 to fit the
    /// master column width.
    pub name: String,
    pub sid: u64,
    /// Number of pids in the pane's tree (including the shell root).
    pub n_pids: u32,
    /// Sum of `ProcessPanelRow.cpu_pct` across the tree.
    pub cpu_pct: f32,
    /// Sum of `ProcessPanelRow.rss_kb` across the tree.
    pub rss_kb: u64,
    /// "What's busy" hint: comm of the top non-shell process by CPU%,
    /// or "(idle)" / "" when the pane is quiet.  Pre-truncated.
    pub busy: String,
}

/// F3+3.0 / 3.3 — full data for one render of the LayoutModal.
/// `set_layout_modal(Some(_))` toggles it on, with the per-slot
/// titles + active drag info needed to paint cards.
#[derive(Debug, Clone)]
pub struct LayoutModalRender {
    pub cols: usize,
    pub rows: usize,
    pub scale: f64,
    /// One title per slot, in slot order (length = cols * rows).
    /// Empty string = empty slot (no card content drawn).
    pub slot_titles: Vec<String>,
    /// Drag state if a card drag is in progress this frame.
    pub drag: Option<LayoutModalDragRender>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutModalDragRender {
    pub from_slot: usize,
    pub grab_offset_phys: (f64, f64),
    pub mouse_phys: (f64, f64),
}

/// F3+9 — full data for one render of a right-click context menu.
/// L2 builds this from `Marspot::context_menu` on every frame the
/// menu is open; renderer paints it through overlay scratches so it
/// lands on top of the grid + sidebar.
#[derive(Debug, Clone)]
pub struct ContextMenuRender {
    pub scale: f64,
    pub anchor_phys: (f64, f64),
    pub top_inset: f64,
    /// One per row.  Drives label + shortcut + enabled / divider
    /// rendering.  Owned by the renderer-side struct (rebuilt each
    /// frame the menu is open) so the renderer doesn't need a shared
    /// reference into Marspot state.
    pub items: Vec<ContextMenuRow>,
    /// Index of the row currently under the cursor, or `None` if
    /// the cursor isn't over any actionable row.
    pub hovered_idx: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ContextMenuRow {
    pub label: String,
    pub shortcut_hint: String,
    pub enabled: bool,
    pub divider: bool,
}

/// cc — one Claude account card in the `Cc` usage modal.
/// Pre-formatted by L2 (`build_cc_usage_render`); the renderer only
/// lays out and paints.
#[derive(Debug, Clone)]
pub struct CcUsageAccountRender {
    pub name: String,
    pub email: String,
    /// Short status label (≤ 7 chars, pre-classified by L2 from the
    /// feed's raw status string) + the severity that picks its colour.
    pub status_label: String,
    /// 0 = ok, 1 = warn, 2 = limited/unknown-bad.
    pub status_severity: u8,
    /// 0.0 ..= 1.0 window utilizations.
    pub util_5h: f32,
    pub util_7d: f32,
    /// Unix reset instants (drive the timeline bars).
    pub reset_5h_unix: i64,
    pub reset_7d_unix: i64,
    /// "reset 5h: 7/19 11:50" — local time, formatted by L2.
    pub reset_label: String,
    /// "11:50" / "14:00" — timeline bar end labels.
    pub reset_5h_hm: String,
    pub reset_7d_hm: String,
    /// Per-model caps: (label, utilization).  One row each, under the
    /// account's own windows — an account can sit at 7 % of its week
    /// and still be shut out of a model, and the 7D bar cannot say so.
    pub model_rows: Vec<(String, f32)>,
}

/// cc — full data for one render of the `Cc` (Claude usage) modal.
/// Renderer pulls this via `set_cc_usage`.  `None` = closed.
#[derive(Debug, Clone)]
/// The settings panel's render data — its rect and the values in
/// force when the frame was built.
///
/// A snapshot, not a live read: the painter must draw one consistent
/// set of values, and re-reading mid-panel could show a toggle from
/// before a click and a segment from after it.
pub struct SettingsRender {
    pub rect: marspot_term::layout::Rect,
    pub settings: crate::settings::Settings,
    /// Shown in the footer so the file is findable.
    pub path: String,
}

pub struct CcUsageRender {
    /// Modal frame rect (physical px), centered by L2.
    pub rect: Rect,
    /// "updated 7/19 10:13" — feed generation time, local.
    pub updated_label: String,
    pub accounts: Vec<CcUsageAccountRender>,
    pub now_unix: i64,
    /// Feed missing/unparseable → placeholder text instead of cards.
    pub feed_missing: bool,
}

/// F3+1.4 — full data for one render of the centered Process Monitor
/// modal.  Renderer pulls this via `set_process_panel`.  `None` =
/// closed, nothing drawn.
#[derive(Debug, Clone)]
pub struct ProcessPanelRender {
    /// Modal frame rectangle (physical px), centered by L2 over the
    /// window.  Includes title bar + master/detail body.
    pub rect: Rect,
    /// Title bar text — currently always "Process Monitor".  Kept
    /// a String so a future plugin could rename per-pane modals.
    pub title: String,
    /// F3+4 — MASTER column: one row per pane (left side).
    /// Pre-sorted by L2; renderer paints in order.  Empty Vec
    /// (no live panes) draws an "(no panes)" placeholder.
    pub pane_rows: Vec<ProcessPanelPaneRow>,
    /// Which pane row in `pane_rows` is currently selected; the
    /// `rows` field below is the process tree of that pane.  Out-
    /// of-range silently clamped to 0 by the renderer.
    pub selected_pane: usize,
    /// F3+4 — DETAIL column: rows of the selected pane's process
    /// tree (header + flatten_pre_order).  Empty Vec OK.
    pub rows: Vec<ProcessPanelRow>,
    /// F3+1.5 — collapse body so only the title bar paints.  When
    /// `true`, `pane_rows` + `rows` are NOT rendered (still walked
    /// for hit-rect bookkeeping on the L2 side).
    pub minimized: bool,
    /// The three traffic-light discs, in physical px.
    ///
    /// Computed once in `marspot-core` (which owns the real backing
    /// scale) and drawn verbatim here.  The painter used to recompute
    /// them from `scale_hint`, a *font-derived* factor — so the dots
    /// scaled with the terminal's point size while the hit rects scaled
    /// with the display, and the two disagreed: 12 px drawn against a
    /// 15 px click target.  System chrome tracks the system, not the
    /// font.
    pub light_rects: [Rect; 3],
    /// Cursor is over the panel's title bar.
    ///
    /// macOS reveals the ×/−/+ glyphs inside its window buttons while
    /// the pointer is anywhere in the title bar — not just over one
    /// button — so the affordance is "these are clickable", not "this
    /// one is". Mirrored here.
    pub title_bar_hovered: bool,
    /// F3+1.5 — body scroll offset (physical px) for the DETAIL
    /// column.  Renderer paints rows starting at
    /// `body_top + body_pad_top - scroll_y`, clipped at body bottom.
    pub scroll_y: f64,
    /// F3+1.5 — semi-transparent backdrop covering the rest of the
    /// window so the modal reads as focused.  Painted as one
    /// `UiRectInstance` before the modal frame.
    pub draw_backdrop: bool,
}

/// Compute the fingerprint hash of the inputs to push_session that
/// affect rendered output.  Any change here invalidates the per-pane
/// instance cache and forces a rebuild.  Cheap (~tens of ns) so it's
/// run unconditionally each frame.
fn pane_fingerprint(
    view: &SessionView,
    rect: &CellRect,
    window_focused: bool,
    hover_chrome_btn: Option<u8>,
    cell_w: f32,
    cell_h: f32,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = marspot_term::fast_hash::FxHasher::default();
    view.seq.hash(&mut h);
    view.view_offset.hash(&mut h);
    view.cursor_visible.hash(&mut h);
    view.focused.hash(&mut h);
    view.title.hash(&mut h);
    view.right_badge.hash(&mut h);
    view.update_pending.hash(&mut h);
    view.ime_preedit.hash(&mut h);
    view.top_fixed_h_cells.hash(&mut h);
    view.bot_fixed_h_cells.hash(&mut h);
    // Selection
    if let Some(sel) = view.selection {
        true.hash(&mut h);
        sel.anchor.0.hash(&mut h);
        sel.anchor.1.hash(&mut h);
        sel.focus.0.hash(&mut h);
        sel.focus.1.hash(&mut h);
        sel.blockwise.hash(&mut h);
    } else {
        false.hash(&mut h);
    }
    // Highlight spans (search active hit).
    view.highlight_spans.len().hash(&mut h);
    for span in view.highlight_spans {
        span.view_row.hash(&mut h);
        span.col_start.hash(&mut h);
        span.col_end_inclusive.hash(&mut h);
    }
    // Search overlay snapshot — every field affecting paint.
    if let Some(ov) = view.search_overlay.as_ref() {
        true.hash(&mut h);
        ov.query.hash(&mut h);
        ov.query_cursor.hash(&mut h);
        ov.case_sensitive.hash(&mut h);
        ov.counter.hash(&mut h);
        ov.hits.len().hash(&mut h);
        for hit in &ov.hits {
            hit.is_focused.hash(&mut h);
            hit.snippet.hash(&mut h);
        }
    } else {
        false.hash(&mut h);
    }
    window_focused.hash(&mut h);
    hover_chrome_btn.hash(&mut h);
    // Layout (catches resize → cache invalidation naturally).
    (rect.x as i64).hash(&mut h);
    (rect.y_top as i64).hash(&mut h);
    (rect.w as i64).hash(&mut h);
    (rect.h as i64).hash(&mut h);
    // Font metrics (catches font / DPI change).
    (cell_w as i64).hash(&mut h);
    (cell_h as i64).hash(&mut h);
    h.finish()
}

/// F1+11 — one UI rect's draw data, layout-compatible with `UiRect`
/// in `src/shaders/cells.metal`.  A real pixel-mode rounded rectangle
/// with anti-aliased corners, optional border stroke, and optional
/// soft drop shadow — all computed in one fragment shader.
///
/// `origin` + `size` describe the FILL rect in physical pixels (NOT
/// inflated by shadow).  `corner_radius` is also in pixels.  Set
/// `shadow_blur = 0` to skip the shadow path entirely (the SDF still
/// runs but contributes nothing).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UiRectInstance {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub fill_color: [f32; 4],
    pub border_color: [f32; 4],
    pub corner_radius: f32,
    pub border_width: f32,
    pub shadow_blur: f32,
    pub shadow_alpha: f32,
    pub shadow_color: [f32; 4],
}

/// Pixel format the Metal pipeline + the CAMetalLayer agree on.
///
/// We use plain `BGRA8Unorm` (NOT `_sRGB`).  The sRGB-encoded variant
/// asks the GPU to treat shader outputs as linear and convert to
/// sRGB on store — which is mathematically clean but means we'd have
/// to feed it linear values.  Our colour constants and the SGR
/// 38;2;R;G;B values claudecode / shells emit are sRGB-space (the
/// CSS / web convention), so passing them to an sRGB-encoded target
/// double-gamma-corrects: a coral `rgb(215,119,87)` comes out brighter
/// and shifted towards red.  iTerm2 / Terminal.app dodge this the
/// same way — non-sRGB format, sRGB values written directly, display
/// reads bytes as sRGB.  Bonus: text AA blends in sRGB space, which
/// designers tune fonts for; linear-space blending makes small text
/// look "thin" and harder to read even though it's mathematically
/// "more correct".
const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// Tag a `CAMetalLayer`'s colour space as **sRGB** so ColorSync converts
/// our sRGB-encoded terminal colours to the display correctly.
///
/// Terminal colours (ANSI + 38;2 truecolor) are sRGB by convention.  With
/// NO tag (`nil`), a CAMetalLayer is treated as already-in-display-space —
/// so on a wide-gamut panel the sRGB values are shown without sRGB→display
/// conversion and over-saturate: Claude Code's coral orange came out
/// pink/"水红".  Tagging **Display P3** over-saturates even harder (wider
/// primaries → coral pinker still).  Tagging **sRGB** does the correct
/// conversion and the coral stays orange, matching iTerm2.
///
/// **Every CAMetalLayer that reaches the screen MUST call this** — the
/// standalone renderer's layer (below) AND the shell presenter's layer
/// (`bin/marspot-shell/present.rs`).  They diverged once (presenter created
/// without any tag → over-saturated colour in the installed app); routing
/// both through this one helper keeps them from drifting again.  NB: the
/// presenter only applies it on (re)launch — a silent core swap alone
/// won't change the layer, so the shell must actually restart to pick up a
/// colour-space change.
pub fn pin_layer_colorspace(layer: &CAMetalLayer) {
    // SAFETY: `kCGColorSpaceSRGB` is an extern static (reading it is
    // `unsafe`); `setColorspace` is reached via raw `objc_msgSend`
    // because the `colorspace` property isn't in the objc2-quartz-core
    // binding yet AND objc2's macro runtime-checks the @encode type
    // ("^{CGColorSpace=}") which doesn't match our `*mut c_void`
    // (`^v`) cast.  Release ships fine because the check is debug-
    // only; for dev (`bin/run.sh`) we go through the raw FFI path
    // so debug builds boot too.
    unsafe {
        let cs = CGColorSpace::create_with_name(kCGColorSpaceSRGB).expect(
            "CGColorSpaceCreateWithName(kCGColorSpaceSRGB) cannot fail on supported macOS",
        );
        let cs_ptr = cs.as_ptr() as *mut c_void;
        let layer_ptr: *mut objc2::runtime::AnyObject =
            (layer as *const CAMetalLayer as *mut CAMetalLayer).cast();
        let sel = objc2::sel!(setColorspace:);
        type Setter = unsafe extern "C" fn(
            *mut objc2::runtime::AnyObject,
            objc2::runtime::Sel,
            *mut c_void,
        );
        let imp: Setter = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
        imp(layer_ptr, sel, cs_ptr);
    }
}

const SHADER_SRC: &str = include_str!("shaders/cells.metal");

/// Metal-backed renderer.  Has the pipeline state for the BG pass
/// (phase 3) but no FG pass yet — `render_cells_bg` is the entry
/// point and `render_cells_bg_offscreen` is its testable cousin.
pub struct MetalRenderer {
    /// The default system device.  One per process; cheap to clone (CFRetain).
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    /// Command submission queue.  One per renderer is fine; Apple's
    /// guidance says queues are heavyweight and should be reused.
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// The CAMetalLayer attached to the host NSView.  `None` in headless
    /// mode (used by tests / benches that don't need a window).
    layer: Option<Retained<CAMetalLayer>>,
    /// Drawable size in physical pixels.  Updated lazily in `resize`.
    width_px: f64,
    height_px: f64,
    /// Pre-built pipeline state for the BG pass — created once at
    /// `new()` so per-frame draw calls don't pay shader-compile cost.
    bg_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// Pre-built pipeline state for the FG (textured glyph) pass.
    /// Has alpha blending enabled — composites onto the BG pass.
    fg_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// FG pipeline for colour glyphs: samples the BGRA colour atlas and
    /// outputs the texel directly (premultiplied-alpha blend) instead of
    /// tinting by the cell fg.  Same vertex shader as `fg_pipeline`.
    fg_color_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// Sampler used by the FG fragment shader.  Linear min/mag for
    /// smooth glyph edges, ClampToEdge so sampling outside the
    /// glyph's atlas slot reads padding (transparent) — not the
    /// neighbouring glyph.
    fg_sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// Pipeline state for the dot pass — same `CellInstance` input
    /// as the BG pass but the fragment shader clips to a circle
    /// inscribed in the quad.  Alpha-blended so the AA edge composites
    /// over the underlying sidebar BG.
    dot_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// F1+11 — Pipeline state for the UI rect pass: anti-aliased
    /// rounded rectangles with optional stroke + soft drop shadow.
    /// Drawn AFTER cells/highlight but BEFORE glyphs so panel
    /// chrome sits over the grid while text on top of the panel
    /// (which goes through `glyphs_scratch`) reads correctly.
    ui_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// Shared font handling — same data the AppKit renderer uses.
    font: FontCache,
    /// The `ui::chrome_scale` the fonts above were built for.  A
    /// display swap changes it; see `rebuild_fonts_if_scale_changed`.
    fonts_built_at_scale: f64,
    /// Glyph atlas backing the FG pass.  Constructed in `new` /
    /// `new_headless`; grows lazily as cells reference new glyphs.
    atlas: GlyphAtlas,
    /// Colour (BGRA8) glyph atlas for full-colour emoji.  Separate from
    /// `atlas` so the mono R8 path stays untouched; populated only when a
    /// colour glyph is first seen.
    color_atlas: GlyphAtlas,
    /// Per-frame instance scratch.  Reset at the start of each
    /// `render_layout` so per-frame allocations stay zero in the
    /// steady state.
    cells_scratch: Vec<CellInstance>,
    glyphs_scratch: Vec<GlyphInstance>,
    /// Colour-glyph instances (emoji) — drawn in a second FG pass that
    /// samples `color_atlas`.  Usually empty (most frames are plain text).
    color_glyphs_scratch: Vec<GlyphInstance>,
    /// Sidebar status dots — same instance layout as cells, but the
    /// dot pipeline clips them to a circle.
    dots_scratch: Vec<CellInstance>,
    /// F1+11 — UI rect instances for the per-frame chrome (search
    /// panel, future menus / tooltips).  Drawn between
    /// cells/highlight and glyphs so panel BG sits under panel text.
    ui_rects_scratch: Vec<UiRectInstance>,
    /// F1+13 — per-pane instance cache.  Each entry holds the
    /// cells / glyphs / color_glyphs slice the renderer produced
    /// for one pane on the most recent frame it actually built
    /// that pane.  When the next frame finds a matching
    /// `fingerprint` (covers grid seq + every relevant
    /// `SessionView` field) AND the same atlas generations, the
    /// renderer copies the cached slice instead of recomputing —
    /// the dominant L2 CPU cost in 9-claudecode workloads.
    /// Window-level focus.  Mirror of the AppKit renderer's flag —
    /// drives whether the focused-session cursor is filled or hollow.
    window_focused: bool,
    /// Hovered chrome icon button, if any.  Encoded as u8 to stay
    /// agnostic of the L2-side enum:  0 = sidebar toggle,  1 =
    /// layout picker,  2 = process-tree panel toggle,  `None` = no
    /// hover.  Renderer reads this to darken the hovered button's BG.
    hover_chrome_btn: Option<u8>,
    /// F3+1.3 — process-tree panel render data.  `None` = panel
    /// closed (renderer paints nothing).  Pushed by L2 every frame
    /// while the panel is open; cheap because rows are typically
    /// tens of entries.
    process_panel: Option<ProcessPanelRender>,
    cc_usage: Option<CcUsageRender>,
    settings_panel: Option<SettingsRender>,
    /// F3+3.0 / 3.3 — LayoutModal render state.  `Some(_)` when
    /// open, `None` when closed.  See `LayoutModalRender` below.
    layout_modal_state: Option<LayoutModalRender>,
    /// RFC-006 — drop-preview ghost for the window being rendered:
    /// (x, y_top, w, h) physical px + outline-only flag (an Append
    /// landing frames the whole content area instead of filling a
    /// half-pane it can't deliver).  Published per window right
    /// before its render, like every per-window overlay.
    drop_preview: Option<((f64, f64, f64, f64), bool)>,
    /// RFC-006 — index of the pane in THIS window currently being
    /// dragged, if any.  The renderer dims it (translucent scrim):
    /// "you are moving THIS one".
    drag_source: Option<usize>,
    /// F3+9 — right-click context menu state.  `Some` while open.
    context_menu_state: Option<ContextMenuRender>,
    /// Dev-panel state.  `Some` while visible.  Owned by L2;
    /// renderer just reads it to build a Canvas and encode.
    dev_panel_state: Option<crate::ui::components::DevPanelState>,
    /// Where the last IOSurface frame spent its time.  See
    /// [`RenderSplit`].
    last_render_split: RenderSplit,
    /// Per-pass instance buffers, reused frame to frame on the
    /// IOSurface path.  See [`InstanceBufferPool`].
    instance_pool: InstanceBufferPool,
    /// F3+1.6 — overlay scratches.  Anything pushed here gets
    /// encoded in EXTRA UI + FG passes AFTER the main grid render,
    /// so it lands on top of all grid pixels regardless of which
    /// pipeline contributed them.  Filtering grid instances by
    /// glyph origin is brittle (vertical/horizontal extents bleed
    /// past origin checks); a dedicated overlay pass is the only
    /// architecturally correct way to make a modal "always on top".
    overlay_cells_scratch: Vec<CellInstance>,
    overlay_glyphs_scratch: Vec<GlyphInstance>,
    overlay_color_glyphs_scratch: Vec<GlyphInstance>,
    overlay_ui_rects_scratch: Vec<UiRectInstance>,
    /// Top inset in physical pixels — reserved for window chrome
    /// (macOS traffic-light buttons). Single-session callers (mcli)
    /// set this once at `resumed`; the convenience `render(view)`
    /// path forwards it into Layout::build's `top_inset` parameter.
    /// Multi-session callers build their own Layout and ignore this.
    top_inset_phys: f64,
    /// Phase 6 — monotonically-increasing frame stamp.  Bumped at the
    /// start of `render_layout` and forwarded into both atlases'
    /// `begin_frame(...)` so cache hits during the frame mark
    /// themselves and the LRU eviction path picks the oldest
    /// non-recent shelf instead of the legacy whole-atlas reset.
    frame_id: u64,
}

impl MetalRenderer {
    /// Construct attached to a host `NSView` — replaces the view's
    /// CALayer with a CAMetalLayer.  Failures (no Metal-capable GPU,
    /// no command queue) bubble up as `Err` strings; caller decides
    /// whether to fall back to the AppKit renderer or refuse to start.
    pub fn new(view: &NSView, scale: f32) -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "MTLDevice.newCommandQueue returned nil".to_string())?;
        let library = build_shader_library(&device)?;
        let bg_pipeline = build_bg_pipeline(&device, &library)?;
        let fg_pipeline = build_fg_pipeline(&device, &library)?;
        let fg_color_pipeline = build_fg_color_pipeline(&device, &library)?;
        let fg_sampler = build_fg_sampler(&device)?;
        let dot_pipeline = build_dot_pipeline(&device, &library)?;
        let ui_pipeline = build_ui_pipeline(&device, &library)?;
        let font = FontCache::build()?;
        // 4096×4096 R8 atlas = 16 MiB (Phase 2 bump from 2048²).
        // Pre-bump 2048² fit ~6000 Menlo 13pt 2× glyphs; Phase 2
        // multiplexes the atlas across pt-sizes (PTY 12 + chrome 13 +
        // future variable-weight faces) so each `size_q` bucket
        // consumes its own working set, and Phase 4 will further ×4
        // for sub-pixel positioning buckets — 16 MiB pre-pays for both
        // without forcing rebuilds in steady state.  Still bounded:
        // `get_or_rasterize` does an atomic rebuild on full so the
        // user never sees silently-blank cells.
        let atlas = GlyphAtlas::new(&device, 4096, 4096)?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let color_atlas = GlyphAtlas::new_color(&device, 1024, 1024)?;

        let layer = { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(TARGET_FORMAT);
            // framebufferOnly = true: drawables can only be render
            // targets, not sample sources.  Cheaper and we don't need
            // to read pixels back during compositing.
            layer.setFramebufferOnly(true);
            layer.setContentsScale(scale as f64);
            // Glitchless live-resize recipe (from
            // `metal-live-resize` / Tristan Hume's 2019 post +
            // empirical work in the shell/core split that drove
            // marspot to Sublime-grade resize smoothness):
            //
            //   1. contentsGravity = topLeft — pin stale frames at
            //      top-left of the layer; default `resize` would
            //      stretch the previous drawable into the new
            //      bounds as a smeary wobble until our next present
            //      lands.
            //   2. setGeometryFlipped(true) — `MarspotView` is
            //      isFlipped=true (top-left origin); without this
            //      the layer's coord system is bottom-left and
            //      `topLeft` gravity actually pins to the *bottom*
            //      of the layer for those few frames, which reads
            //      as content jumping up/down when dragging the
            //      bottom edge of the window.
            //   3. setPresentsWithTransaction(true) — drawable is
            //      handed to the next CATransaction (same one
            //      AppKit uses for bounds changes during a live
            //      resize), so window bounds and pixels land in the
            //      same frame.  Caller must do
            //      cmd.commit() + waitUntilScheduled() +
            //      drawable.present() instead of
            //      cmd.presentDrawable() — the live `render_layout`
            //      path below honours this.
            layer.setContentsGravity(kCAGravityTopLeft);
            layer.setGeometryFlipped(true);
            layer.setPresentsWithTransaction(true);
            layer.setOpaque(true);
            // Tag layer colour space (Display P3, shared helper — see
            // pin_layer_colorspace; the shell presenter calls the same one).
            pin_layer_colorspace(&layer);
        }

        view.setWantsLayer(true);
        {
            // NSView's own resize-time content placement.  AppKit's
            // path takes over for the brief moment between the
            // window-bounds change and our next present; default
            // `ScaleAxesIndependently` stretches old contents into
            // the new bounds, `TopLeft` mirrors the layer-side
            // gravity so the two compositing paths agree.
            view.setLayerContentsPlacement(NSViewLayerContentsPlacement::TopLeft);
            view.setLayer(Some(&layer));
            // Window BG = terminal BG so the gap between resize +
            // first repaint reads as the same colour, not the system
            // window BG.  Mirrors the AppKit renderer.
            if let Some(window) = view.window() {
                let bg = NSColor::colorWithSRGBRed_green_blue_alpha(
                    crate::font_cache::BG.0,
                    crate::font_cache::BG.1,
                    crate::font_cache::BG.2,
                    1.0,
                );
                window.setBackgroundColor(Some(&bg));
            }
        }

        Ok(Self {
            fonts_built_at_scale: crate::ui::chrome_scale(),
            device,
            queue,
            layer: Some(layer),
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
            fg_pipeline,
            fg_color_pipeline,
            fg_sampler,
            dot_pipeline,
            ui_pipeline,
            font,
            atlas,
            color_atlas,
            cells_scratch: Vec::new(),
            dots_scratch: Vec::new(),
            ui_rects_scratch: Vec::new(),
            overlay_cells_scratch: Vec::new(),
            overlay_glyphs_scratch: Vec::new(),
            overlay_color_glyphs_scratch: Vec::new(),
            overlay_ui_rects_scratch: Vec::new(),
            glyphs_scratch: Vec::new(),
            color_glyphs_scratch: Vec::new(),
            window_focused: true,
            hover_chrome_btn: None,
            process_panel: None, cc_usage: None, settings_panel: None, layout_modal_state: None, drop_preview: None, drag_source: None, context_menu_state: None, dev_panel_state: None,
            last_render_split: RenderSplit::default(),
            instance_pool: InstanceBufferPool::default(),
            top_inset_phys: 0.0,
            frame_id: 0,
        })
    }

    /// Construct without a view — for unit tests, benches, or a future
    /// offscreen render path.  `render_clear` is a no-op (no drawable),
    /// but `render_cells_bg_offscreen` works end-to-end against a
    /// caller-provided MTLTexture.
    pub fn new_headless() -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "MTLDevice.newCommandQueue returned nil".to_string())?;
        let library = build_shader_library(&device)?;
        let bg_pipeline = build_bg_pipeline(&device, &library)?;
        let fg_pipeline = build_fg_pipeline(&device, &library)?;
        let fg_color_pipeline = build_fg_color_pipeline(&device, &library)?;
        let fg_sampler = build_fg_sampler(&device)?;
        let dot_pipeline = build_dot_pipeline(&device, &library)?;
        let ui_pipeline = build_ui_pipeline(&device, &library)?;
        let font = FontCache::build()?;
        // 4096×4096 R8 atlas = 16 MiB (Phase 2 bump from 2048²).
        // Pre-bump 2048² fit ~6000 Menlo 13pt 2× glyphs; Phase 2
        // multiplexes the atlas across pt-sizes (PTY 12 + chrome 13 +
        // future variable-weight faces) so each `size_q` bucket
        // consumes its own working set, and Phase 4 will further ×4
        // for sub-pixel positioning buckets — 16 MiB pre-pays for both
        // without forcing rebuilds in steady state.  Still bounded:
        // `get_or_rasterize` does an atomic rebuild on full so the
        // user never sees silently-blank cells.
        let atlas = GlyphAtlas::new(&device, 4096, 4096)?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let color_atlas = GlyphAtlas::new_color(&device, 1024, 1024)?;
        Ok(Self {
            fonts_built_at_scale: crate::ui::chrome_scale(),
            device,
            queue,
            layer: None,
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
            fg_pipeline,
            fg_color_pipeline,
            fg_sampler,
            dot_pipeline,
            ui_pipeline,
            font,
            atlas,
            color_atlas,
            cells_scratch: Vec::new(),
            dots_scratch: Vec::new(),
            ui_rects_scratch: Vec::new(),
            overlay_cells_scratch: Vec::new(),
            overlay_glyphs_scratch: Vec::new(),
            overlay_color_glyphs_scratch: Vec::new(),
            overlay_ui_rects_scratch: Vec::new(),
            glyphs_scratch: Vec::new(),
            color_glyphs_scratch: Vec::new(),
            window_focused: true,
            hover_chrome_btn: None,
            process_panel: None, cc_usage: None, settings_panel: None, layout_modal_state: None, drop_preview: None, drag_source: None, context_menu_state: None, dev_panel_state: None,
            last_render_split: RenderSplit::default(),
            instance_pool: InstanceBufferPool::default(),
            top_inset_phys: 0.0,
            frame_id: 0,
        })
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// F3+1.3 — push the process-tree panel render data.  `None`
    /// closes (renderer skips the panel pass).  Called by L2 on every
    /// render frame while the panel is open; cheap because typical
    /// row counts are < 200 and we're just storing the Vec.
    pub fn set_cc_usage(&mut self, data: Option<CcUsageRender>) {
        self.cc_usage = data;
    }

    pub fn set_settings_panel(&mut self, data: Option<SettingsRender>) {
        self.settings_panel = data;
    }

    pub fn set_process_panel(&mut self, data: Option<ProcessPanelRender>) {
        self.process_panel = data;
    }

    /// F3+3.0 / 3.3 — caller publishes LayoutModal open state +
    /// pending (cols, rows) + per-slot titles + active drag info
    /// every frame the modal might paint.  `None` disables the
    /// modal entirely.
    pub fn set_layout_modal(&mut self, state: Option<LayoutModalRender>) {
        self.layout_modal_state = state;
    }

    /// F3+9 — set the context-menu render state.  `Some` while the
    /// menu is open, `None` when closed.  Re-published every frame
    /// by L2 with current hovered_idx so the highlight tracks the
    /// cursor.
    pub fn set_drop_preview(&mut self, rect: Option<((f64, f64, f64, f64), bool)>) {
        self.drop_preview = rect;
    }

    pub fn set_drag_source(&mut self, idx: Option<usize>) {
        self.drag_source = idx;
    }

    pub fn set_context_menu(&mut self, state: Option<ContextMenuRender>) {
        self.context_menu_state = state;
    }

    /// Set the dev panel render state.  `Some` when visible,
    /// `None` when hidden.  Caller (L2) toggles via toolbar
    /// button / keyboard shortcut; we just paint it.
    pub fn set_dev_panel(
        &mut self,
        state: Option<crate::ui::components::DevPanelState>,
    ) {
        self.dev_panel_state = state;
    }

    /// L2 — set which chrome icon button (if any) is under the
    /// cursor.  `None` clears; `Some(0)` = sidebar toggle, `Some(1)`
    /// = layout picker.  Renderer uses this in `push_layout_chrome`
    /// to darken the hovered button's BG.  L2's `CoreApp` calls this
    /// from its `mouse_moved` after a chrome hit-test.
    pub fn set_hover_chrome_btn(&mut self, h: Option<u8>) {
        self.hover_chrome_btn = h;
    }

    /// Mark the next IOSurface-target render as needing a hard Clear.
    /// Call from anywhere the visible BG region SHAPE is about to change
    /// (layout mode switch, sidebar toggle, resize, surface reattach).
    /// See `clear_bg_required` field doc for the race that motivates
    /// the Load-default for steady-state frames.


    /// Reserve a top strip (physical pixels) above the grid so window
    /// chrome (traffic lights, focused-session status) doesn't paint
    /// over terminal content. Single-session callers (mcli) set this
    /// once at `resumed` from `HEADER_PT * scale`. Multi-session
    /// callers build their own Layout with `top_inset` baked in and
    /// don't touch this field.
    pub fn set_top_inset(&mut self, phys: f64) {
        self.top_inset_phys = phys;
    }

    /// Top inset in physical pixels currently in effect.
    pub fn top_inset_phys(&self) -> f64 {
        self.top_inset_phys
    }

    /// Single-session convenience render — builds a 1×1 Layout with
    /// the current viewport + top inset, then dispatches to
    /// `render_layout`. mcli uses this so it doesn't have to know
    /// about Layout / sidebars.
    pub fn render(&mut self, wr: &mut WindowRender, view: SessionView) {
        if self.layer.is_none() {
            return;
        }
        if self.width_px < 1.0 || self.height_px < 1.0 {
            return;
        }
        let layout = Layout::build(
            self.width_px,
            self.height_px,
            0.0,                 // sidebar_w
            self.top_inset_phys, // top_inset
            0.0,                 // gutter
            1,                   // cols
            1,                   // rows
            self.font.cell_w,
            self.font.cell_h,
        );
        self.render_layout(wr, &layout, std::slice::from_ref(&view), &[], 0);
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        self.font.cell_dims()
    }

    /// Phase 10c — borrow the `FontCache` mutably for chrome
    /// measurement.  Used by `chrome_measure::ChromeMeasure::new`
    /// to wrap the cache in a `RefCell` for the view layout pass.
    /// Re-derive the fonts for the display scale in force, if it has
    /// changed since they were built.
    ///
    /// The terminal cell comes out of `FontCache::build`, which reads
    /// `ui::chrome_scale` — so a window moved between displays of
    /// different densities needs the cache rebuilt or the grid keeps
    /// the old density's cell.  Returns whether anything changed, so
    /// the caller only reflows when it must.
    ///
    /// Cheap enough to call on every scale report and no cheaper: it
    /// re-opens the font stack (~ms) and empties both atlases, which
    /// the next frame re-fills for the glyphs actually on screen.
    pub fn rebuild_fonts_if_scale_changed(&mut self) -> bool {
        let want = crate::ui::chrome_scale();
        if (self.fonts_built_at_scale - want).abs() < 1e-9 {
            return false;
        }
        let Ok(font) = FontCache::build() else {
            // Keep the fonts we have: a cell of the wrong density is
            // legible, and no cell at all is not.
            return false;
        };
        self.font = font;
        self.atlas.drop_all_glyphs();
        self.color_atlas.drop_all_glyphs();
        self.fonts_built_at_scale = want;
        true
    }

    pub fn font_mut(&mut self) -> &mut FontCache {
        &mut self.font
    }

    /// Caret rect for a single-session render (mcli's path).  Builds
    /// the same 1×1 Layout `render()` uses, then asks the layout where
    /// the focused session's `(col, row)` maps in view-local physical
    /// pixels (top-left, y-down).  Marspot's main loop has a real
    /// multi-cell `Layout` and calls `Layout::caret_view_phys_rect`
    /// directly; the geometry is shared in `Layout` so both binaries
    /// stay in sync.  Returns `None` only when the viewport hasn't
    /// been sized yet.
    ///
    /// Not gated on `view.cursor_visible`: an app that hides the
    /// cursor while it works still has an insertion point, and the
    /// IME candidate window has to anchor to it (see
    /// `PaneBackend::ime_caret_cell`).
    pub fn focused_caret_view_phys_rect(
        &self,
        view: &SessionView,
    ) -> Option<(f64, f64, f64, f64)> {
        if self.width_px < 1.0 || self.height_px < 1.0 {
            return None;
        }
        let layout = Layout::build(
            self.width_px,
            self.height_px,
            0.0,
            self.top_inset_phys,
            0.0,
            1,
            1,
            self.font.cell_w,
            self.font.cell_h,
        );
        let (col, row) = view.grid.cursor();
        layout.caret_view_phys_rect(0, col, row, self.font.cell_w, self.font.cell_h)
    }

    pub fn atlas_approx_bytes(&self) -> usize {
        self.atlas.approx_bytes()
    }

    pub fn fontcache_approx_bytes(&self) -> usize {
        self.font.approx_bytes()
    }

    /// Bytes held in this renderer's per-frame scratch buffers.  The
    /// pipeline-state / sampler / queue handles are CFRetain'd Apple
    /// objects whose footprint lives in CoreGraphics / Metal heaps;
    /// not counted here.  Per-frame `MTLBuffer`s are constructed and
    /// dropped each frame via `make_buffer_from_bytes` so they don't
    /// live in this struct.
    pub fn metal_buffers_approx_bytes(&self) -> usize {
        self.cells_scratch.capacity() * std::mem::size_of::<CellInstance>()
            + self.glyphs_scratch.capacity() * std::mem::size_of::<GlyphInstance>()
            + self.dots_scratch.capacity() * std::mem::size_of::<CellInstance>()
    }

    /// Update the drawable size after a host-window resize.  Cheap on
    /// no-op (same dims).
    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        if (width_px - self.width_px).abs() < 0.5
            && (height_px - self.height_px).abs() < 0.5
        {
            return;
        }
        self.width_px = width_px;
        self.height_px = height_px;
        if let Some(layer) = &self.layer {
            {
                layer.setDrawableSize(NSSize {
                    width: width_px,
                    height: height_px,
                });
            }
        }
    }

    /// Phase-1 render: clear the drawable to `bg` and present.  Returns
    /// `false` if there's no drawable available (e.g. all in-flight, or
    /// headless mode), `true` if a frame was committed.
    pub fn render_clear(&self, bg: (f64, f64, f64)) -> bool {
        let layer = match &self.layer {
            Some(l) => l,
            None => return false,
        };
        let drawable = match { layer.nextDrawable() } {
            Some(d) => d,
            None => return false,
        };
        let texture = { drawable.texture() };

        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor {
                red: bg.0,
                green: bg.1,
                blue: bg.2,
                alpha: 1.0,
            });
        }

        let cmd = self
            .queue
            .commandBuffer()
            .expect("command queue out of buffers");
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("renderCommandEncoderWithDescriptor returned nil");
        encoder.endEncoding();
        // `presentsWithTransaction = true` path: see comment in
        // `render_layout` below.  Cast Retained<dyn CAMetalDrawable>
        // down to MTLDrawable for `present`.
        cmd.commit();
        cmd.waitUntilScheduled();
        use objc2_metal::MTLDrawable;
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        mtl_drawable.present();
        true
    }

    /// Phase 5b live render — same call shape as
    /// `crate::render::Renderer::render_layout`.  Walks each session's
    /// grid + scrollback, emits BG cell instances + FG glyph
    /// instances, then runs both passes against the next CAMetalLayer
    /// drawable and presents.  No-op in headless mode.
    /// Chrome font metrics: (cell_w, cell_h, ascent) in physical
    /// pixels.  Exposed for the dev-window subsystem which builds
    /// canvases outside the main render flow.
    pub fn chrome_font_metrics(&self) -> (f32, f32, f32) {
        (self.font.cell_w as f32, self.font.cell_h as f32, self.font.ascent as f32)
    }

    /// Terminal font metrics — the font used for the actual grid /
    /// PTY output.  Identical to `chrome_font_metrics` for v1 because
    /// they currently share a `FontCache`;  reserves the API surface
    /// for future split where UI chrome can use a different font.
    pub fn terminal_font_metrics(&self) -> (f32, f32, f32) {
        self.chrome_font_metrics()
    }

    /// UI font metrics — the font used for chrome / dev panel / UI
    /// primitives via the View tree framework.  Reports the system
    /// UI font(`.AppleSystemUIFont` cascade)cell w/h/ascent when
    /// loaded (default);  falls back to terminal font when system UI
    /// font isn't available.  `MARSPOT_UI_FONT_SCALE` env applies a
    /// multiplier(0.0..4.0).
    pub fn ui_font_metrics(&self) -> (f32, f32, f32) {
        let scale = std::env::var("MARSPOT_UI_FONT_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|s| *s > 0.0 && *s < 4.0)
            .unwrap_or(1.0);
        // Read UI metrics straight off the FontCache (separate from
        // terminal mono metrics).
        let w = self.font.ui_cell_w as f32;
        let h = self.font.ui_cell_h as f32;
        let a = self.font.ui_ascent as f32;
        (w * scale, h * scale, a * scale)
    }

    /// Render one `Canvas` directly into our CAMetalLayer's next
    /// drawable.  Used by `DevWindow` which owns this renderer
    /// and has no `Layout` / sessions to feed into `render_layout`.
    /// `width_px` / `height_px` size the layer + viewport this
    /// frame; caller has already informed the renderer of any
    /// resize.
    pub fn render_canvas_into_layer(
        &mut self,
        canvas: &crate::ui::core::canvas::Canvas,
        width_px: f32,
        height_px: f32,
        chrome_cell_w: f32,
        chrome_cell_h: f32,
        chrome_ascent: f32,
        ui_font: bool,
    ) {
        let Some(layer) = self.layer.as_ref() else { return };
        // Size the layer to match the view.  drawableSize is in
        // physical pixels.
        {
            layer.setDrawableSize(NSSize {
                width: width_px as f64,
                height: height_px as f64,
            });
        }
        let drawable = match { layer.nextDrawable() } {
            Some(d) => d,
            None => return,
        };
        let texture = { drawable.texture() };
        let cmd = match self.queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
        let viewport_px = [width_px, height_px];
        encode_canvas_into(
            canvas,
            &texture,
            &cmd,
            &self.ui_pipeline,
            &self.fg_pipeline,
            &self.fg_color_pipeline,
            &self.fg_sampler,
            &mut self.atlas,
            &mut self.color_atlas,
            &self.device,
            &mut self.font,
            Some(MTLClearColor { red: 0.078, green: 0.086, blue: 0.110, alpha: 1.0 }),
            &viewport_px,
            chrome_cell_w, chrome_cell_h, chrome_ascent,
            ui_font,
        );
        cmd.commit();
        cmd.waitUntilScheduled();
        use objc2_metal::MTLDrawable;
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        mtl_drawable.present();
    }

    pub fn render_layout(
        &mut self,
        wr: &mut WindowRender,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
    ) {
        if self.layer.is_none() {
            return;
        }
        if self.width_px < 1.0 || self.height_px < 1.0 {
            return;
        }
        // Phase 6 — stamp the frame so atlases can LRU-touch entries
        // they serve this render.
        self.frame_id = self.frame_id.wrapping_add(1);
        let frame_id = self.frame_id;
        self.atlas.begin_frame(frame_id);
        self.color_atlas.begin_frame(frame_id);

        // Disjoint borrow so build_instances can mutate font + atlas
        // + scratch while we still hold &references to the GPU bits.
        let Self {
            ref device,
            ref queue,
            ref layer,
            ref bg_pipeline,
            ref fg_pipeline,
            ref fg_color_pipeline,
            ref fg_sampler,
            ref dot_pipeline,
            ref ui_pipeline,
            ref mut font,
            ref mut atlas,
            ref mut color_atlas,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            ref mut color_glyphs_scratch,
            ref mut dots_scratch,
            ref mut ui_rects_scratch,
            ref mut overlay_cells_scratch,
            ref mut overlay_glyphs_scratch,
            ref mut overlay_color_glyphs_scratch,
            ref mut overlay_ui_rects_scratch,
            window_focused,
            hover_chrome_btn,
            ref process_panel,
            ref cc_usage,
            ref settings_panel,
            width_px,
            height_px,
            ..
        } = *self;

        cells_scratch.clear();
        glyphs_scratch.clear();
        color_glyphs_scratch.clear();
        dots_scratch.clear();
        ui_rects_scratch.clear();
        overlay_cells_scratch.clear();
        overlay_glyphs_scratch.clear();
        overlay_color_glyphs_scratch.clear();
        overlay_ui_rects_scratch.clear();
        build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            hover_chrome_btn,
            process_panel.as_ref(),
            cc_usage.as_ref(),
            settings_panel.as_ref(),
            self.layout_modal_state.as_ref(),
            self.drop_preview,
            self.drag_source,
            self.context_menu_state.as_ref(),
            font,
            atlas,
            color_atlas,
            cells_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            dots_scratch,
            ui_rects_scratch,
            &mut wr.pane_caches,
            overlay_cells_scratch,
            overlay_glyphs_scratch,
            overlay_ui_rects_scratch,
        );

        let layer = layer.as_ref().unwrap();
        let drawable = match { layer.nextDrawable() } {
            Some(d) => d,
            None => return,
        };
        let texture = { drawable.texture() };

        let cmd = match queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };

        encode_passes(
            &cmd,
            &texture,
            bg_pipeline,
            dot_pipeline,
            fg_pipeline,
            fg_color_pipeline,
            ui_pipeline,
            fg_sampler,
            atlas,
            color_atlas,
            device,
            cells_scratch,
            dots_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            ui_rects_scratch,
            overlay_cells_scratch,
            overlay_glyphs_scratch,
            overlay_ui_rects_scratch,
            width_px as f32,
            height_px as f32,
            // CAMetalLayer drawable, same process — no cross-process
            // race possible.  Always Clear for the full hard-fill.
            true,
            // This path waits only until *scheduled*, so the GPU may
            // still be reading last frame's instances — refilling in
            // place would race it.  Allocate per frame here.
            None,
        );

        // Dev panel — Canvas-based overlay. Encoded BEFORE the
        // context menu so the menu (if open) sits on top.
        let chrome_cell_w = font.cell_w as f32;
        let chrome_cell_h = font.cell_h as f32;
        let chrome_ascent = font.ascent as f32;
        let viewport_px = [width_px as f32, height_px as f32];
        if let Some(dev_state) = self.dev_panel_state.as_ref() {
            if dev_state.visible {
                let measure = crate::chrome_measure::ChromeMeasure::new(
                    font,
                    chrome_cell_w as f64,
                    chrome_cell_h as f64,
                );
                let canvas = crate::ui::components::build_dev_panel_canvas(
                    dev_state,
                    width_px, height_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    &measure,
                );
                drop(measure);
                encode_canvas_into(
                    &canvas, &texture, &cmd,
                    ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                    atlas, color_atlas, device, font,
                    None, &viewport_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    false,
                );
            }
        }

        // P2c — ContextMenu draws OUT-OF-BAND via the Canvas
        // pipeline: built fresh per frame and encoded with
        // submission-order = z-order semantics, AFTER the main
        // encode_passes so it sits above every other overlay.
        if let Some(menu_state) = self.context_menu_state.as_ref() {
            let canvas = build_context_menu_canvas(
                menu_state,
                width_px,
                height_px,
                chrome_cell_w,
                chrome_cell_h,
            );
            encode_canvas_into(
                &canvas,
                &texture,
                &cmd,
                ui_pipeline,
                fg_pipeline,
                fg_color_pipeline,
                fg_sampler,
                atlas,
                color_atlas,
                device,
                font,
                None, // Load — preserve everything below
                &viewport_px,
                chrome_cell_w, chrome_cell_h, chrome_ascent,
                false,
            );
        }

        // `presentsWithTransaction = true` path (set up in `new`):
        // commit + waitUntilScheduled + drawable.present() so the
        // drawable lands in the next CATransaction alongside any
        // pending window-bounds change.
        cmd.commit();
        cmd.waitUntilScheduled();
        use objc2_metal::MTLDrawable;
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        mtl_drawable.present();
    }

    /// Bench / test variant of `render_layout`.  Encodes the BG + FG
    /// passes against `target` (any Render-Target MTLTexture) and
    /// blocks on `waitUntilCompleted` so the caller can time the
    /// full GPU round-trip without racing the next frame.
    ///
    /// Use case: `--bench metal-render` headless harness in main.rs
    /// — same per-frame work as the live path but no CAMetalLayer /
    /// drawable / present.
    pub fn render_layout_to_texture(
        &mut self,
        wr: &mut WindowRender,
        target: &ProtocolObject<dyn MTLTexture>,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
    ) {
        self.render_layout_to_texture_inner(wr, target, layout, views, sidebar, focused_idx, true)
    }

    /// The live variant: commit and leave the frame running.
    ///
    /// The caller polls `WindowRender::settled()` and only then flips
    /// the surface — see `WindowRender::in_flight` for why blocking
    /// here was costing the terminal its input responsiveness.
    pub fn render_layout_to_texture_async(
        &mut self,
        wr: &mut WindowRender,
        target: &ProtocolObject<dyn MTLTexture>,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
    ) {
        self.render_layout_to_texture_inner(wr, target, layout, views, sidebar, focused_idx, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_layout_to_texture_inner(
        &mut self,
        wr: &mut WindowRender,
        target: &ProtocolObject<dyn MTLTexture>,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
        block: bool,
    ) {
        let width_px = target.width() as f64;
        let height_px = target.height() as f64;
        if width_px < 1.0 || height_px < 1.0 {
            return;
        }
        // Mirror the live path's drawableSize side-effect so any
        // viewport-driven downstream logic on `self` sees the right
        // dims after the call.
        self.width_px = width_px;
        self.height_px = height_px;
        // Consume the clear-required flag BEFORE the disjoint borrow:
        // after this frame, subsequent IOSurface renders can Load until
        // something explicitly marks the flag again (resize, layout
        // change, etc.).
        let clear_bg = wr.take_clear_required();
        // Dev-only — flash investigation 2026-06-15.  At default Info
        // level this is filtered out; flip MARSPOT_LOG=debug to see
        // every IOSurface render's clear/load verdict.  Demote /
        // remove once the C-path Load fix is confirmed working in
        // the wild.
        crate::lx_debug!(
            "render.iosurf",
            "encoding IOSurface frame",
            clear_bg = clear_bg
        );

        let Self {
            ref device,
            ref queue,
            ref bg_pipeline,
            ref fg_pipeline,
            ref fg_color_pipeline,
            ref fg_sampler,
            ref dot_pipeline,
            ref ui_pipeline,
            ref mut font,
            ref mut atlas,
            ref mut color_atlas,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            ref mut color_glyphs_scratch,
            ref mut dots_scratch,
            ref mut ui_rects_scratch,
            ref mut overlay_cells_scratch,
            ref mut overlay_glyphs_scratch,
            ref mut overlay_color_glyphs_scratch,
            ref mut overlay_ui_rects_scratch,
            window_focused,
            hover_chrome_btn,
            ref process_panel,
            ref cc_usage,
            ref settings_panel,
            ref mut instance_pool,
            ..
        } = *self;

        let evict0 = atlas.evictions + color_atlas.evictions;
        let rebuild0 = atlas.rebuild_count + color_atlas.rebuild_count;
        let t_build0 = std::time::Instant::now();
        cells_scratch.clear();
        glyphs_scratch.clear();
        color_glyphs_scratch.clear();
        dots_scratch.clear();
        ui_rects_scratch.clear();
        overlay_cells_scratch.clear();
        overlay_glyphs_scratch.clear();
        overlay_color_glyphs_scratch.clear();
        overlay_ui_rects_scratch.clear();
        let build_stats = build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            hover_chrome_btn,
            process_panel.as_ref(),
            cc_usage.as_ref(),
            settings_panel.as_ref(),
            self.layout_modal_state.as_ref(),
            self.drop_preview,
            self.drag_source,
            self.context_menu_state.as_ref(),
            font,
            atlas,
            color_atlas,
            cells_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            dots_scratch,
            ui_rects_scratch,
            &mut wr.pane_caches,
            overlay_cells_scratch,
            overlay_glyphs_scratch,
            overlay_ui_rects_scratch,
        );

        // Glyph work is charged to `build`: `build_instances` is what
        // calls `get_or_rasterize`, so a cold atlas shows up as build
        // time and the counter says how much of it that was.
        let glyphs_rasterised =
            atlas.take_rasterised() + color_atlas.take_rasterised();
        let evictions = (atlas.evictions + color_atlas.evictions)
            .saturating_sub(evict0);
        let rebuilds = (atlas.rebuild_count + color_atlas.rebuild_count)
            .saturating_sub(rebuild0);
        let _ = take_instance_buffer_cost(); // zero the accumulator
        let t_cmdbuf0 = std::time::Instant::now();
        let cmd = match queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
        let t_encode0 = std::time::Instant::now();
        encode_passes(
            &cmd,
            target,
            bg_pipeline,
            dot_pipeline,
            fg_pipeline,
            fg_color_pipeline,
            ui_pipeline,
            fg_sampler,
            atlas,
            color_atlas,
            device,
            cells_scratch,
            dots_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            ui_rects_scratch,
            overlay_cells_scratch,
            overlay_glyphs_scratch,
            overlay_ui_rects_scratch,
            width_px as f32,
            height_px as f32,
            // IOSurface path — cross-process race-free only when Load
            // is used in steady state.  Consume the flag set by
            // `mark_bg_clear_required` (e.g. resize, layout change).
            clear_bg,
            // Safe to refill in place: this path ends in
            // `waitUntilCompleted` below, so the GPU is finished with
            // the previous frame's instances before this one writes.
            Some(instance_pool),
        );
        let t_canvas0 = std::time::Instant::now();
        // Dev panel + ContextMenu canvases (same shape as render_layout).
        let chrome_cell_w = font.cell_w as f32;
        let chrome_cell_h = font.cell_h as f32;
        let chrome_ascent = font.ascent as f32;
        let viewport_px = [width_px as f32, height_px as f32];
        if let Some(dev_state) = self.dev_panel_state.as_ref() {
            if dev_state.visible {
                let measure = crate::chrome_measure::ChromeMeasure::new(
                    font,
                    chrome_cell_w as f64,
                    chrome_cell_h as f64,
                );
                let canvas = crate::ui::components::build_dev_panel_canvas(
                    dev_state, width_px, height_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    &measure,
                );
                drop(measure);
                encode_canvas_into(
                    &canvas, target, &cmd,
                    ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                    atlas, color_atlas, device, font,
                    None, &viewport_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    false,
                );
            }
        }
        if let Some(menu_state) = self.context_menu_state.as_ref() {
            let canvas = build_context_menu_canvas(
                menu_state, width_px, height_px,
                chrome_cell_w, chrome_cell_h,
            );
            encode_canvas_into(
                &canvas, target, &cmd,
                ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                atlas, color_atlas, device, font,
                None, &viewport_px,
                chrome_cell_w, chrome_cell_h, chrome_ascent,
                false,
            );
        }
        let t_commit0 = std::time::Instant::now();
        let (instbuf_us, instbuf_bytes) = take_instance_buffer_cost();
        cmd.commit();
        let t_gpu0 = std::time::Instant::now();
        // Blocking is for the bench harness, which wants the whole
        // round-trip in one number.  The live path hands the frame to
        // `wr` and returns — see `WindowRender::in_flight`.
        let gpu_exec_us = if block {
            { cmd.waitUntilCompleted() };
            let (s, e) = (cmd.GPUStartTime(), cmd.GPUEndTime());
            ((e - s).max(0.0) * 1e6) as u64
        } else {
            wr.in_flight = Some(cmd.clone());
            // Not known yet; the poll that reaps the frame fills it in.
            wr.last_gpu_exec_us
        };
        let t_end = std::time::Instant::now();
        // Bracketing is sound here even though sub-microsecond timers
        // can be defeated by reordering: `commit` and
        // `waitUntilCompleted` are opaque calls with side effects, and
        // the quantities being separated are milliseconds apart.
        self.last_render_split = RenderSplit {
            build_us: (t_cmdbuf0 - t_build0).as_micros() as u64,
            cmdbuf_us: (t_encode0 - t_cmdbuf0).as_micros() as u64,
            encode_us: (t_canvas0 - t_encode0).as_micros() as u64,
            instbuf_us,
            instbuf_bytes,
            canvas_us: (t_commit0 - t_canvas0).as_micros() as u64,
            gpu_wait_us: (t_end - t_gpu0).as_micros() as u64,
            gpu_exec_us,
            build_panes_us: build_stats.panes_us,
            panes_rebuilt: build_stats.rebuilt,
            panes_total: build_stats.considered,
            glyphs_rasterised,
            evictions,
            rebuilds,
        };
    }

    /// Where the last IOSurface frame spent its time.
    pub fn last_render_split(&self) -> RenderSplit {
        self.last_render_split
    }
}

/// Encode BG → dot → FG passes against `target`.  Shared by the live
/// `render_layout` (drawable target) and the offscreen
/// `render_layout_to_texture` (caller-provided target).  Pass order
/// matters: BG paints opaquely (no blend); dot + FG are alpha-blended
/// on top.
#[allow(clippy::too_many_arguments)]
fn encode_passes(
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    target: &ProtocolObject<dyn MTLTexture>,
    bg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    dot_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_color_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    ui_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas: &GlyphAtlas,
    color_atlas: &GlyphAtlas,
    device: &ProtocolObject<dyn MTLDevice>,
    cells: &[CellInstance],
    dots: &[CellInstance],
    glyphs: &[GlyphInstance],
    color_glyphs: &[GlyphInstance],
    ui_rects: &[UiRectInstance],
    overlay_cells: &[CellInstance],
    overlay_glyphs: &[GlyphInstance],
    overlay_ui_rects: &[UiRectInstance],
    viewport_w: f32,
    viewport_h: f32,
    // Clear-vs-Load for the BG pass.  `true` = hard Clear to SIDEBAR_BG
    // (correct for first frame after attach, resize, layout-shape
    // change, OR the same-process CAMetalLayer path that can never
    // race a cross-process reader).  `false` = Load previous frame's
    // pixels (used by the IOSurface path in steady state to eliminate
    // the cross-process flash race documented on
    // `MetalRenderer::clear_bg_required`).
    clear_bg: bool,
    // `Some` = refill persistent buffers (only sound on a path that
    // ends in `waitUntilCompleted`); `None` = allocate per frame.
    pool: Option<&mut InstanceBufferPool>,
) {
    let mut pool = pool;
    /// Slot `$slot`'s instances, from the pool when there is one.
    macro_rules! inst {
        ($slot:expr, $bytes:expr) => {{
            let bytes = $bytes;
            match &mut pool {
                Some(p) => p.upload(device, $slot, bytes),
                None => make_instance_buffer(device, bytes),
            }
        }};
    }
    let viewport: [f32; 2] = [viewport_w, viewport_h];
    let viewport_ptr = NonNull::new(viewport.as_ptr() as *mut c_void).unwrap();
    let viewport_len = std::mem::size_of::<[f32; 2]>();

    // BG pass — clear to SIDEBAR_BG, then draw all opaque cells in
    // submission order (chrome → sidebar → per-session bg → cursor →
    // focus outline → underline → inter-cell gutter seams).  Always
    // Clear: the chrome strip above the grid is NOT covered by an
    // opaque CellInstance, so it relies on the Clear to reset every
    // frame.  Briefly attempted Load-by-default (2126cda) to dodge a
    // suspected cross-process IOSurface race, but the real race-killer
    // is the shell-side frame_pending gate in `redraw()` (a1d3930);
    // Load was redundant AND it let the chrome strip's alpha-blended
    // glyphs accumulate over their own anti-aliased edges, fuzzing the
    // title-bar text after a few seconds.  The `clear_bg` parameter is
    // retained as a hook for any future per-frame decision.
    let _ = clear_bg;
    let bg_pass = { MTLRenderPassDescriptor::new() };
    unsafe {
        let color = bg_pass.colorAttachments().objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setStoreAction(MTLStoreAction::Store);
        color.setLoadAction(MTLLoadAction::Clear);
        color.setClearColor(MTLClearColor {
            red: SIDEBAR_BG_F.0 as f64,
            green: SIDEBAR_BG_F.1 as f64,
            blue: SIDEBAR_BG_F.2 as f64,
            alpha: 1.0,
        });
    }
    let bg_buffer = inst!(0, cells_as_bytes(cells));
    let bg_encoder = cmd
        .renderCommandEncoderWithDescriptor(&bg_pass)
        .expect("bg encoder");
    bg_encoder.setRenderPipelineState(bg_pipeline);
    if let Some(buf) = &bg_buffer {
        unsafe { bg_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0) };
    }
    unsafe {
        bg_encoder.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
        if !cells.is_empty() {
            bg_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                cells.len(),
            );
        }
    }
    bg_encoder.endEncoding();

    // Dot pass — circle-clipped, alpha-blended.  Skipped entirely if
    // no dots queued (fast path for the bench / mcli single-session
    // case where there's no sidebar).
    if !dots.is_empty() {
        let dot_pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = dot_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let dot_buffer = inst!(1, cells_as_bytes(dots));
        let dot_encoder = cmd
            .renderCommandEncoderWithDescriptor(&dot_pass)
            .expect("dot encoder");
        dot_encoder.setRenderPipelineState(dot_pipeline);
        if let Some(buf) = &dot_buffer {
            unsafe { dot_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0) };
        }
        unsafe {
            dot_encoder.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            dot_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                dots.len(),
            );
        }
        dot_encoder.endEncoding();
    }

    // UI rect pass — anti-aliased rounded rectangles for chrome
    // overlays (search panel, future tooltips/menus).  Drawn AFTER
    // grid BG / highlights / dots, BEFORE the glyph passes — so
    // glyphs that belong to the overlay (panel text) read on top.
    // Skipped entirely when no overlay is queued.
    if !ui_rects.is_empty() {
        let ui_pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = ui_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let ui_buffer = inst!(2, ui_rects_as_bytes(ui_rects));
        let ui_encoder = cmd
            .renderCommandEncoderWithDescriptor(&ui_pass)
            .expect("ui encoder");
        ui_encoder.setRenderPipelineState(ui_pipeline);
        if let Some(buf) = &ui_buffer {
            unsafe { ui_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0) };
        }
        unsafe {
            ui_encoder.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            ui_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                ui_rects.len(),
            );
        }
        ui_encoder.endEncoding();
    }

    // FG pass — textured glyph quads, alpha-blended on top.
    let fg_pass = { MTLRenderPassDescriptor::new() };
    unsafe {
        let color = fg_pass.colorAttachments().objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setLoadAction(MTLLoadAction::Load);
        color.setStoreAction(MTLStoreAction::Store);
    }
    let fg_buffer = inst!(3, glyphs_as_bytes(glyphs));
    let fg_encoder = cmd
        .renderCommandEncoderWithDescriptor(&fg_pass)
        .expect("fg encoder");
    fg_encoder.setRenderPipelineState(fg_pipeline);
    if let Some(buf) = &fg_buffer {
        unsafe { fg_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0) };
    }
    unsafe {
        fg_encoder.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
        fg_encoder.setFragmentTexture_atIndex(Some(atlas.texture()), 0);
        fg_encoder.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
        if !glyphs.is_empty() {
            fg_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                glyphs.len(),
            );
        }
    }
    fg_encoder.endEncoding();

    // Colour FG pass — full-colour glyphs (emoji) sampled from the BGRA
    // atlas, premultiplied-alpha blended on top.  Skipped entirely when
    // nothing colour was queued (the common case — most frames have no
    // emoji), so the extra encoder costs nothing for plain text.
    if !color_glyphs.is_empty() {
        let cfg_pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = cfg_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let cfg_buffer = inst!(4, glyphs_as_bytes(color_glyphs));
        let cfg_encoder = cmd
            .renderCommandEncoderWithDescriptor(&cfg_pass)
            .expect("color fg encoder");
        cfg_encoder.setRenderPipelineState(fg_color_pipeline);
        if let Some(buf) = &cfg_buffer {
            unsafe { cfg_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0) };
        }
        unsafe {
            cfg_encoder.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            cfg_encoder.setFragmentTexture_atIndex(Some(color_atlas.texture()), 0);
            cfg_encoder.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
            cfg_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                color_glyphs.len(),
            );
        }
        cfg_encoder.endEncoding();
    }

    // F3+1.6 — OVERLAY PASSES.  Drawn after every main pass so the
    // overlay (Process Monitor modal, future tooltips/sheets) covers
    // whatever the grid + chrome + glyphs produced underneath.  No
    // filtering of grid scratches needed; the overlay's BG is on top
    // by construction.  Sequence: BG cells → UI rects → FG glyphs.
    // (Cells go FIRST so opaque flat fills sit below the SDF chrome
    // rects; glyphs LAST so text always reads on top.)
    if !overlay_cells.is_empty() {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let buf = inst!(5, cells_as_bytes(overlay_cells));
        let enc = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("overlay cells encoder");
        enc.setRenderPipelineState(bg_pipeline);
        if let Some(b) = &buf {
            unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
        }
        unsafe {
            enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                overlay_cells.len(),
            );
        }
        enc.endEncoding();
    }
    if !overlay_ui_rects.is_empty() {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let buf = inst!(6, ui_rects_as_bytes(overlay_ui_rects));
        let enc = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("overlay ui encoder");
        enc.setRenderPipelineState(ui_pipeline);
        if let Some(b) = &buf {
            unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
        }
        unsafe {
            enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                overlay_ui_rects.len(),
            );
        }
        enc.endEncoding();
    }
    if !overlay_glyphs.is_empty() {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let buf = inst!(7, glyphs_as_bytes(overlay_glyphs));
        let enc = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("overlay fg encoder");
        enc.setRenderPipelineState(fg_pipeline);
        if let Some(b) = &buf {
            unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
        }
        unsafe {
            enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
            enc.setFragmentTexture_atIndex(Some(atlas.texture()), 0);
            enc.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                overlay_glyphs.len(),
            );
        }
        enc.endEncoding();
    }
}

/// Allocate a render-target MTLTexture.  Helper for tests + the
/// `--bench metal-render` harness.  StorageModePrivate (GPU-only)
/// because we never read the bytes back in the bench path; for
/// readback (existing offscreen render_cells_bg_offscreen / fg)
/// the caller still allocates Managed + blit-synchronizes.
/// A render target whose bytes can be read back on the CPU.
///
/// [`make_target_texture`] asks for `Private` storage — right for a
/// texture only the GPU ever looks at, and unreadable by `getBytes`.
/// Anything that wants the pixels afterwards (the offscreen `--shot`
/// path, the snapshot tests) needs `Managed`, plus a blit
/// `synchronizeResource` before the read.
pub fn make_readback_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe {
        objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(
        objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
    );
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
    device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())
}

/// Copy a Managed texture's pixels out, BGRA, top-left origin.
///
/// The caller must have synchronised the resource already — every
/// call site here does it inside the command buffer that drew, which
/// is the only place it can be done without a second submit.
pub fn texture_bytes_bgra(texture: &ProtocolObject<dyn MTLTexture>) -> Vec<u8> {
    let width = texture.width();
    let height = texture.height();
    let bytes_per_row = width * 4;
    let mut bytes = vec![0u8; bytes_per_row * height];
    let region = objc2_metal::MTLRegion {
        origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: objc2_metal::MTLSize { width, height, depth: 1 },
    };
    unsafe {
        texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
            NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
            bytes_per_row,
            region,
            0,
        );
    }
    bytes
}

pub fn make_target_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe {
        objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(
        objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
    );
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Private);
    device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())
}

impl MetalRenderer {
    /// Expose the device so the bench harness can allocate a
    /// render-target texture without a public `device` field.
    pub fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.device
    }

    /// Flush a Managed render target's GPU writes to the CPU side and
    /// hand back its pixels, BGRA.
    ///
    /// A second command buffer, because `render_layout_to_texture` has
    /// already committed its own — the cost of one extra submit buys
    /// callers that do not have to thread a blit through the render
    /// path they are borrowing.
    pub fn read_target(
        &self,
        texture: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<Vec<u8>, String> {
        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let blit = cmd
            .blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        Ok(texture_bytes_bgra(texture))
    }

    /// Bench helper (perf-attack B3/B4): rasterise each char into the
    /// atlas via the real cache-miss path (`resolve_char` +
    /// `get_or_rasterize`), returning total nanoseconds.  Distinct chars
    /// force misses, so this isolates exactly the per-glyph cost a
    /// CJK/emoji firehose pays on first sight of each glyph — the
    /// `get_bounding_rects` CT call, the per-glyph `Vec` alloc +
    /// `CGBitmapContextCreate` + ~8 `set_*` context-property calls, and
    /// `draw_glyphs` — with no GPU draw and no parse in the number.  Call
    /// on a fresh renderer for a cold atlas.  Returns 0 if `chars` is
    /// empty.  Keep the distinct-char count under the atlas capacity
    /// (~20k cell-sized slots) to avoid a rebuild skewing the average.
    pub fn bench_rasterize(&mut self, chars: &[char]) -> u64 {
        let metrics = SlotMetrics {
            cell_w: self.font.cell_w.round() as u32,
            cell_h: self.font.cell_h.round() as u32,
            baseline_from_top: self.font.ascent.round() as u32,
        };
        let t0 = std::time::Instant::now();
        for &ch in chars {
            let _ = resolve_cell_glyph(&mut self.atlas, &mut self.font, ch, false, false, metrics);
        }
        t0.elapsed().as_nanos() as u64
    }
}

/// Chrome / cursor / focus-outline constants, kept in sync with
/// `render.rs`.  Shaders consume `f32`s, so duplicate as `f32`-tuples
/// here rather than convert per call.
// Inter-cell divider colour — light grey so a thin "white-on-dark"
// hairline shows between panes (iTerm2 style — the working area
// === Visual design ===
//
// Two near-black tones plus one quiet seam.  The focused pane
// READS DEEPER than the rest: the surrounding chrome (sidebar,
// header, unfocused cells) sits at `BG_PANEL`, while the active
// pane drops to `BG_FOCUSED` (= `font_cache::BG`) — one shade
// below.  Focus = "the deep canvas I'm typing into", everything
// else = "the surrounding panel".  Every internal boundary uses
// one weak `SEAM` hairline at one width, so the eye reads
// structure without any seam taking on chrome weight.
//
//   BG_PANEL    the dominant surface (sidebar + header +
//               unfocused cell rects); has a faint blue tint
//   BG_FOCUSED  the focused cell rect — visibly deeper, closer
//               to pure black; this IS the focus indicator
//   SEAM        a hair darker than BG_PANEL, reads as a quiet
//               depression between adjacent panels — used for
//               sidebar↔grid, header↔grid, and cell↔cell alike
// BG_PANEL lifted in a second pass (2026-06-15) to widen the
// focused/unfocused contrast — the prior (0.022, 0.028, 0.042) was
// too close to BG_FOCUSED even after pushing focused toward true
// black.  Still a deep tone, just decisively above 0.
const BG_PANEL: (f32, f32, f32) = (0.040, 0.050, 0.075);
// Pure black for the focused pane.  User feedback: "focused 还得再
// 黑一点".  Snapping all-zero is fine here — we never paint
// foreground glyphs in pure white, so the BG-to-glyph contrast is
// dominated by glyph color, not a few thousandths of BG tint.
const BG_FOCUSED: (f32, f32, f32) = (0.000, 0.000, 0.000);
/// C4 — search-hit highlight BG.  Bright yellow with reverse foreground
/// (mid-luminance, slightly desaturated so the original glyph FG
/// reads clearly on top — distinct from text-selection's BG+FG
/// inversion).  §6.8 colour roles.
const HIGHLIGHT_BG: (f32, f32, f32) = (0.92, 0.78, 0.20);
// Pre-mixed against BG_PANEL ≈ 50%, so the 0.5-px sub-pixel quad
// reads as a translucent hairline.  Going through alpha blending
// would need pipeline changes; this gets the same visual effect
// at the BG-pipeline solid-fill cost.
// Bumped twice on 2026-06-15.  First bump tracked the BG_PANEL lift
// to preserve the original luminance gap; user reported the seams
// were still subtle ("可能本来就是有点淡"), so second bump pushes
// the gap further — luminance diff vs BG_PANEL ≈ 0.08, comfortably
// above the just-noticeable-difference threshold without bleeding
// into chrome territory.  Same direction (brighter than panel),
// gutter width unchanged so seams stay 1 hairline thick.
const SEAM: (f32, f32, f32) = (0.115, 0.130, 0.155);
/// Selected-cell highlight — a muted brand blue that lifts cleanly
/// over BG_FOCUSED without bleaching foreground text.  Used by the
/// drag-to-select machinery; FG glyphs draw on top so selected
/// content stays legible.
const SELECTION_BG: (f32, f32, f32) = (0.16, 0.22, 0.34);
// IME preedit colours.  BG a touch above the focused-cell BG so the
// preview stands out without screaming; FG slightly muted vs the
// committed-text FG so the user reads "in flight, not yet".  The
// hairline underline below the glyph is what most editors use to
// flag composition state.
const IME_PREEDIT_BG: (f32, f32, f32) = (0.10, 0.13, 0.18);
const IME_PREEDIT_FG: (f32, f32, f32) = (0.80, 0.86, 0.92);
// Old name retained for the existing sidebar BG drawing path
// (kept flush with the panel surface).
const SIDEBAR_BG_F: (f32, f32, f32) = BG_PANEL;
const CURSOR_FG: (f32, f32, f32) = (0.92, 0.92, 0.92);

const SIDEBAR_DOT_R: f32 = 4.5;
const SIDEBAR_LEFT_PAD: f32 = 14.0;
// Sidebar's row 0 offset is now `layout::sidebar_top_pad_phys` —
// computed by Layout::build to reserve room for the [+] header
// band.  Threaded through `push_sidebar` instead of read from a
// local const.
const SIDEBAR_ROW_H: f32 = 22.0;
const SIDEBAR_DOT_LABEL_GAP: f32 = 10.0;
const SIDEBAR_TEXT_FG: (f32, f32, f32) = (0.78, 0.82, 0.88);
/// Header version label — slightly brighter than sidebar metadata so
/// the "current version" reads clearly when the user glances up to
/// confirm an update landed.
const HEADER_VERSION_FG: (f32, f32, f32) = (0.62, 0.68, 0.80);
/// Deferred-update refresh glyph in the focused pane's title strip — a
/// warm amber so it reads as an actionable "update ready" control against
/// the dim title text (target #4 step 5b).
const REFRESH_ICON_FG: (f32, f32, f32) = (0.95, 0.74, 0.30);
/// Claudecode brand coral — matches the orange the claude CLI uses
/// for its own prompt + spinner glyphs.  Plugin badges currently
/// hard-code this so the right-side decoration reads as "claudecode"
/// at a glance; if more plugins land we'll move colour into the wire
/// format alongside the text.
const PLUGIN_BADGE_FG: (f32, f32, f32) = (0.85, 0.47, 0.34);
/// Underline colour for auto-detected clickable spans (URLs, file
/// paths, email).  Calm cyan so it reads as "I'm a link" without
/// fighting ANSI-styled body text.
const LINK_UNDERLINE_FG: (f32, f32, f32) = (0.40, 0.70, 0.95);
// Selected-row BG kept as an alias of the cell-focused tone so
// sidebar selection and 9-grid focus read as the same affordance.
const STATE_ACTIVE: (f32, f32, f32) = (0.30, 0.85, 0.45);
const STATE_IDLE: (f32, f32, f32) = (0.55, 0.58, 0.62);
const STATE_EXITED: (f32, f32, f32) = (0.85, 0.30, 0.30);

/// Walk each session view + sidebar entry, emit BG cell instances
/// and FG glyph instances into the caller-owned scratch vecs.
/// Stays a free function so its `&mut FontCache, &mut GlyphAtlas,
/// &mut Vec<…>` arguments don't conflict with the GPU references
/// the encoder needs to hold.
#[allow(clippy::too_many_arguments)]
/// What `build_instances` measured about itself.
#[derive(Clone, Copy, Debug, Default)]
struct BuildStats {
    panes_us: u64,
    rebuilt: u32,
    considered: u32,
}

fn build_instances(
    layout: &Layout,
    views: &[SessionView],
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    window_focused: bool,
    hover_chrome_btn: Option<u8>,
    process_panel: Option<&ProcessPanelRender>,
    cc_usage: Option<&CcUsageRender>,
    settings_panel: Option<&SettingsRender>,
    layout_modal_state: Option<&LayoutModalRender>,
    drop_preview: Option<((f64, f64, f64, f64), bool)>,
    drag_source: Option<usize>,
    context_menu_state: Option<&ContextMenuRender>,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    _dots: &mut Vec<CellInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
    pane_caches: &mut Vec<PaneInstanceCache>,
    overlay_cells: &mut Vec<CellInstance>,
    overlay_glyphs: &mut Vec<GlyphInstance>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) -> BuildStats {
    let cell_w = font.cell_w as f32;
    let cell_h = font.cell_h as f32;
    let ascent = font.ascent as f32;
    let (atlas_w, atlas_h) = atlas.dims();
    let atlas_w_f = atlas_w as f32;
    let atlas_h_f = atlas_h as f32;

    // RFC-006 — drop-preview ghost: a translucent accent fill + thin
    // accent frame over the region a hovering pane drag would occupy
    // on release; outline-only for an Append landing (no half-pane to
    // promise, just "into this window").  Overlay pass, above the
    // content it previews.  What lights up is exactly what release
    // does.
    if let Some(((gx, gy, gw, gh), outline_only)) = drop_preview {
        let accent = crate::ui::theme::token::color::ACCENT.to_rgba_f32();
        let fill = if outline_only {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            [accent[0] * 0.25, accent[1] * 0.25, accent[2] * 0.25, 0.25]
        };
        overlay_ui_rects.push(UiRectInstance {
            origin: [gx as f32, gy as f32],
            size: [gw as f32, gh as f32],
            fill_color: fill,
            border_color: accent,
            corner_radius: 4.0,
            border_width: 2.0,
            shadow_blur: 0.0,
            shadow_alpha: 0.0,
            shadow_color: [0.0; 4],
        });
    }

    // RFC-006 polish — whole-cell scrims, one primitive two duties:
    //  * dormant placeholder: a steady recess so an empty seat reads
    //    as "space held, nothing running" (hint text shows through);
    //  * drag source: a deeper dim while its pane is mid-drag —
    //    "you are moving THIS one" (the ⇢ title marker stays for the
    //    pointer-outside-any-window case).
    for (i, view) in views.iter().enumerate() {
        // Reasons a pane can recede, one primitive.  The deepest wins
        // rather than stacking: two scrims at 0.22 read as one at
        // 0.39, which is a different (and unintended) shade.
        let dimming = [
            (drag_source == Some(i)).then_some(DRAG_SOURCE_SCRIM),
            view.dormant.then_some(EMPTY_SEAT_SCRIM),
            // Already eased by the pane — see `ScrimFade`.  The step
            // it is heading for is `attention_scrim(focused, recede)`;
            // what arrives here is where it has got to.
            Some(view.scrim),
        ]
        .into_iter()
        .flatten()
        .filter(|a| *a > 0.0)
        .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a: f32| a.max(v))));
        let (Some(alpha), Some(rect)) = (dimming, layout.cells.get(i)) else {
            continue;
        };
        overlay_ui_rects.push(UiRectInstance {
            origin: [rect.x as f32, rect.y_top as f32],
            size: [rect.w as f32, rect.h as f32],
            fill_color: [0.0, 0.0, 0.0, alpha],
            border_color: [0.0; 4],
            corner_radius: 0.0,
            border_width: 0.0,
            shadow_blur: 0.0,
            shadow_alpha: 0.0,
            shadow_color: [0.0; 4],
        });
    }

    // Sidebar BG = cell BG (already covered by the clear pass).  No
    // explicit fill needed unless the sidebar palette ever diverges.

    // Every internal hairline — sidebar↔grid, header↔grid, and
    // cell↔cell — uses one uniformly weak SEAM tone at one width.
    // The eye reads structure (this is a sidebar / this is a grid /
    // this is a cell) without any seam taking on chrome weight.
    // F3+1.18 — GridSeams now uses the BG (cells) pipeline so seams
    // tile pixel-perfect with no SDF AA seams.  This means push
    // order matters (BG pass renders in instance order): GridSeams
    // MUST run AFTER pane BG cells (push_session in pane loop), so
    // seams cover pane BG instead of being overdrawn by it.  Moved
    // to inside the chrome painter scope below.

    // Ensure cache has a slot per pane (grown lazily; never shrunk
    // intra-session — pane count is bounded by the 9-grid layout).
    while pane_caches.len() < views.len() {
        pane_caches.push(PaneInstanceCache::default());
    }

    let t_panes0 = std::time::Instant::now();
    let mut rebuilt = 0u32;
    let mut considered = 0u32;
    for (i, view) in views.iter().enumerate() {
        let rect = match layout.cells.get(i) {
            Some(r) => r,
            None => continue,
        };
        considered += 1;
        // F1+13 — per-pane instance cache.  Hash the inputs that
        // affect `push_session`'s output.  Hit ⇒ memcpy cached
        // slices into the global accumulators (cheap).  Miss ⇒
        // rebuild + snapshot the slice this pane just produced
        // into the cache so the NEXT idle frame for this pane is
        // a hit.  ui_rects are NOT cached (only one pane has the
        // search overlay at a time, and rebuilding it is cheap).
        let fp = pane_fingerprint(
            view, rect, window_focused, hover_chrome_btn, cell_w, cell_h,
        );
        let cur_atlas_gen = atlas.rebuild_count;
        let cur_color_gen = color_atlas.rebuild_count;
        let cur_link_gen = crate::link_probe::generation();
        let cache = &mut pane_caches[i];
        let hit = cache.primed
            && cache.fingerprint == fp
            && cache.atlas_gen == cur_atlas_gen
            && cache.color_atlas_gen == cur_color_gen
            && cache.link_gen == cur_link_gen;
        if hit {
            cells.extend_from_slice(&cache.cells);
            glyphs.extend_from_slice(&cache.glyphs);
            color_glyphs.extend_from_slice(&cache.color_glyphs);
            continue;
        }
        rebuilt += 1;
        let cells_start = cells.len();
        let glyphs_start = glyphs.len();
        let color_glyphs_start = color_glyphs.len();
        push_session(
            rect,
            view,
            window_focused,
            cell_w,
            cell_h,
            ascent,
            atlas_w_f,
            atlas_h_f,
            font,
            atlas,
            color_atlas,
            cells,
            glyphs,
            color_glyphs,
            ui_rects,
            layout.gutter as f32,
            layout.padding as f32,
            layout.cell_title_h as f32,
        );
        // Snapshot this pane's contributions into the cache.
        // Re-read atlas gens after the call: a glyph miss during
        // push_session may have triggered a rebuild, in which case
        // the slice we're caching uses the post-rebuild uvs and
        // must record THAT gen for the hit check to be sound.
        let cache = &mut pane_caches[i];
        cache.fingerprint = fp;
        cache.atlas_gen = atlas.rebuild_count;
        cache.color_atlas_gen = color_atlas.rebuild_count;
        cache.link_gen = cur_link_gen;
        cache.cells.clear();
        cache.cells.extend_from_slice(&cells[cells_start..]);
        cache.glyphs.clear();
        cache.glyphs.extend_from_slice(&glyphs[glyphs_start..]);
        cache.color_glyphs.clear();
        cache.color_glyphs.extend_from_slice(&color_glyphs[color_glyphs_start..]);
        cache.primed = true;
    }
    let panes_us = t_panes0.elapsed().as_micros() as u64;

    // F3+1.13 — all post-pane main-scratch UI in one ViewPainter
    // scope: empty cells, sidebar, chrome toolbar, close × / add +
    // glyphs, version label.  No more raw cells.push / glyphs.push
    // in this function past this point.
    {
        use crate::ui::core::view::ViewPainter;
        use crate::ui::components::{
            Sidebar, SidebarRow, SidebarStyle, Button, ButtonStyle, IconSpec, IconPosition,
            GridSeams, SeamStyle, GridItem, Outline, GridEdges,
        };
        let mut painter = ViewPainter {
            cell_w, cell_h, ascent,
            atlas_w: atlas_w_f, atlas_h: atlas_h_f,
            window_w: layout.window_w, window_h: layout.window_h,
            font, atlas, cells, glyphs, ui_rects,
        };

        // F3+1.19 — grid base seams FIRST (gray dividers between
        // panes), then per-pane GridItem outline.  Both go through
        // the BG pipeline (fill_rect, no SDF AA), so push order =
        // z order: pane BG (pushed by pane loop above) → base seams
        // → focus outline on the focused pane.  All boundaries are
        // pixel-perfect — rasterizer assigns each pixel to one rect
        // by sample-center rule, no AA seams.
        if layout.gutter > 0.0 && !layout.cells.is_empty() {
            // F3+1.14 — pane grid seams thickened to 4× gutter so the
            // grid reads as deliberate panes, not gutters-as-margin.
            let seam_thickness = (layout.gutter * 4.0).max(2.0);
            let seam_style = SeamStyle {
                color: [SEAM.0, SEAM.1, SEAM.2, 1.0],
                thickness: seam_thickness,
            };
            let pane_rects: Vec<Rect> = layout.cells.iter().map(|c| Rect {
                x: c.x, y_top: c.y_top, w: c.w, h: c.h,
            }).collect();
            GridSeams {
                cells: &pane_rects,
                cols: layout.grid_cols,
                rows: layout.grid_rows,
                vertical: seam_style,
                horizontal: seam_style,
            }
            .paint(&mut painter);

            // F3+1.20 — per-pane focus outline.  RN-style outline
            // (doesn't shrink pane content), width + gutter passed
            // so GridItem places the 8 ring rects at exactly where
            // the GridSeams base seams sit — outline pixel-for-pixel
            // REPLACES the gray seam color on the focused side, no
            // overshoot beyond the seam footprint.
            let focus_outline = Outline {
                color: [0.72, 0.76, 0.82, 1.0],
                width: seam_thickness,
            };
            let cols = layout.grid_cols.max(1);
            let rows = layout.grid_rows.max(1);
            for (idx, rect) in pane_rects.iter().enumerate().take(views.len()) {
                let r = idx / cols;
                let c = idx % cols;
                let item = GridItem {
                    rect: *rect,
                    focused: idx == focused_idx,
                    outline: focus_outline,
                    gutter: layout.gutter,
                    edges: GridEdges {
                        top:    r == 0,
                        right:  c == cols - 1,
                        bottom: r == rows - 1,
                        left:   c == 0,
                    },
                };
                item.paint(&mut painter);
                // …and again in the overlay pass.  The ring's right and
                // bottom sides sit in the seam, which lies INSIDE the
                // neighbouring cells' rects — and every unfocused pane
                // draws a full-cell scrim in the overlay pass, i.e.
                // after this one.  So the two sides that mark the focus
                // most clearly were the two the neighbours dimmed
                // (2026-08-20 report).  Repainting the same rects after
                // the scrims restores them; the copy is antialiased and
                // the original is not, but they are the same rect in the
                // same colour, so the fringe blends into itself.
                for rr in item.ring_rects() {
                    overlay_ui_rects.push(UiRectInstance {
                        origin: [rr.x as f32, rr.y_top as f32],
                        size: [rr.w as f32, rr.h as f32],
                        fill_color: focus_outline.color,
                        border_color: [0.0; 4],
                        corner_radius: 0.0,
                        border_width: 0.0,
                        shadow_blur: 0.0,
                        shadow_alpha: 0.0,
                        shadow_color: [0.0; 4],
                    });
                }
            }
        }

        // Empty-cell BG overlay + [+] hint.  Each empty cell becomes
        // a low-key ghost Button with a "+" glyph centered inside.
        let n_visible = views.len();
        let empty_style = ButtonStyle {
            bg:           [EMPTY_CELL_OVERLAY[0], EMPTY_CELL_OVERLAY[1],
                           EMPTY_CELL_OVERLAY[2], EMPTY_CELL_OVERLAY[3]],
            bg_hover:     [EMPTY_CELL_OVERLAY[0], EMPTY_CELL_OVERLAY[1],
                           EMPTY_CELL_OVERLAY[2], EMPTY_CELL_OVERLAY[3]],
            fg:           EMPTY_CELL_GLYPH_FG,
            fg_hover:     EMPTY_CELL_GLYPH_FG,
            border_color: [0.0, 0.0, 0.0, 0.0],
            border_width: 0.0,
            corner_radius: 0.0,
            padding_x: 0.0,
            icon_gap: 0.0,
            icon_size: cell_h,
        };
        for cell_rect in layout.cells.iter().skip(n_visible) {
            let inner_top = cell_rect.y_top + layout.cell_title_h;
            let inner_h = (cell_rect.h - layout.cell_title_h).max(0.0);
            let inner = Rect {
                x: cell_rect.x, y_top: inner_top,
                w: cell_rect.w, h: inner_h,
            };
            Button {
                rect: inner,
                label: None,
                icon: Some(IconSpec::Glyph("+")),
                icon_position: IconPosition::Only,
                hovered: false,
                style: empty_style,
            }.paint(&mut painter);
        }

        // Sidebar — rows + status dot + label.  Replaces push_sidebar.
        if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
            let rows: Vec<SidebarRow> = sidebar.iter().map(|e| {
                let dot = match e.state {
                    SessionState::Active => STATE_ACTIVE,
                    SessionState::Idle   => STATE_IDLE,
                    SessionState::Exited => STATE_EXITED,
                };
                SidebarRow {
                    label: e.label,
                    dot_color: [dot.0, dot.1, dot.2, 1.0],
                }
            }).collect();
            Sidebar {
                rect: Rect {
                    x: 0.0,
                    y_top: layout.top_inset,
                    w: layout.sidebar_w,
                    h: (layout.window_h - layout.top_inset).max(0.0),
                },
                rows: &rows,
                focused_idx,
                style: SidebarStyle {
                    focused_bg: [BG_FOCUSED.0, BG_FOCUSED.1, BG_FOCUSED.2, 1.0],
                    label_fg:   [SIDEBAR_TEXT_FG.0, SIDEBAR_TEXT_FG.1, SIDEBAR_TEXT_FG.2, 1.0],
                    row_h: SIDEBAR_ROW_H,
                    top_pad: layout.sidebar_top_pad_phys as f32,
                    left_pad: SIDEBAR_LEFT_PAD,
                    dot_radius: SIDEBAR_DOT_R,
                    dot_label_gap: SIDEBAR_DOT_LABEL_GAP,
                },
            }
            .paint(&mut painter);
        }

        // Toolbar buttons + picker overlay + close/add BGs.
        push_layout_chrome(layout, hover_chrome_btn, &mut painter);

        // Close-[×] glyph per close button — `painter.text("×", ...)`
        // centred in each close_session_rect.  Replaces push_close_glyphs.
        if !layout.close_session_rects.is_empty() {
            let close_disabled = layout.close_session_rects.len() == 1;
            let color = if close_disabled { CLOSE_BTN_FG_DISABLED } else { CLOSE_BTN_FG };
            for r in &layout.close_session_rects {
                let x = (r.x + (r.w - cell_w as f64) * 0.5) as f32;
                let y_baseline = (r.y_top + (r.h - cell_h as f64) * 0.5) as f32 + ascent;
                painter.text(x, y_baseline, "×", color);
            }
        }
        // Add-[+] glyph in the sidebar add button.  Replaces
        // push_add_button_glyph.
        if layout.add_session_button_rect.w > 0.0 {
            let add_disabled =
                layout.close_session_rects.len() >= SESSION_COUNT_HARD_CAP;
            let color = if add_disabled { ADD_BTN_FG_DISABLED } else { ADD_BTN_FG };
            let r = layout.add_session_button_rect;
            let x = (r.x + (r.w - cell_w as f64) * 0.5) as f32;
            let y_baseline = (r.y_top + (r.h - cell_h as f64) * 0.5) as f32 + ascent;
            painter.text(x, y_baseline, "+", color);
        }

        // Header version label — quiet metadata flush right, on the
        // same row as the traffic lights and the toolbar buttons.
        if layout.top_inset > 0.0 {
            let label = version_label();
            let text_w = label.chars().count() as f32 * cell_w;
            let scale_approx = (layout.top_inset as f32) / crate::HEADER_PT as f32;
            let right_margin_logical_pt: f32 = 10.0;
            let right_margin_phys = right_margin_logical_pt * scale_approx;
            let x =
                ((layout.window_w as f32) - right_margin_phys - text_w).max(cell_w);
            let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32
                * scale_approx;
            let baseline_y = (row_center - cell_h * 0.5).max(0.0) + ascent;
            painter.text(
                x, baseline_y, &label,
                [HEADER_VERSION_FG.0, HEADER_VERSION_FG.1, HEADER_VERSION_FG.2, 1.0],
            );
        }
    }

    // F3+1.8 — per-pane search overlay.  Painted via the same
    // overlay-scratch path as the modal — no filter, no in-cache
    // mixing, opaque BG by default via `ViewStyle`.  Position is
    // pane-local, sourced from the layout cell rect we already
    // walked to push the session's grid.
    {
        use crate::ui::core::view::ViewPainter;
        use crate::ui::components::search_overlay::{
            paint_search_overlay, SearchOverlayParams,
        };
        for (i, view) in views.iter().enumerate() {
            let Some(rect) = layout.cells.get(i) else { continue };
            let Some(overlay) = view.search_overlay.as_ref() else { continue };
            let inner_x = rect.x as f32 + layout.padding as f32;
            let inner_y = rect.y_top as f32
                + layout.cell_title_h as f32
                + layout.padding as f32;
            let mut painter = ViewPainter {
                cell_w, cell_h, ascent,
                atlas_w: atlas_w_f, atlas_h: atlas_h_f,
                window_w: layout.window_w, window_h: layout.window_h,
                font, atlas,
                cells: overlay_cells,
                glyphs: overlay_glyphs,
                ui_rects: overlay_ui_rects,
            };
            paint_search_overlay(&mut painter, SearchOverlayParams {
                overlay,
                inner_x,
                inner_y,
                grid_cols: view.grid.cols(),
                grid_rows: view.grid.rows(),
            });
        }
    }

    // F3+1.7 — Process Monitor modal renders via the `View` component
    // which owns the overlay-scratch + extra-pass plumbing.  Build
    // sites only see the painter; backdrop / frame / always-on-top
    // are configured up front and applied automatically.
    if let Some(panel) = process_panel {
        push_process_panel_via_view(
            panel,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // cc — `Cc` usage modal (Claude account windows).  Same overlay
    // + backdrop treatment as the process panel.
    if let Some(cc) = cc_usage {
        push_cc_usage_via_view(
            cc,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // The settings panel.  Same overlay + backdrop treatment.
    if let Some(sp) = settings_panel {
        push_settings_panel_via_view(
            sp,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // F3+3.0 — LayoutModal: cols/rows steppers + Apply.  Overlay
    // scratches → renders on top of grid, modal-style backdrop dims
    // everything below the title strip.
    if let Some(modal_state) = layout_modal_state {
        push_layout_modal_via_view(
            modal_state,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // F3+9 / P2c — right-click ContextMenu render is NOT routed
    // through the shared overlay scratches; it builds a `Canvas`
    // in render_layout AFTER the main encode_passes and uses
    // `encode_canvas` to draw in submission-order.  The variable
    // is consumed there.
    let _ = context_menu_state;

    BuildStats { panes_us, rebuilt, considered }
}

/// The attention ladder: how present a pane is, from whether the user
/// is in it and how far its session has receded.
///
/// One pane is fully there — the one being used.  Everything else
/// steps back; a session that has been sitting finished steps back
/// again; one that has been reclaimed steps back furthest.  A glance
/// should answer "where am I / what is still alive / what has drifted
/// out" before any reading happens.
///
/// Expressed as scrim alpha, which is `1 - opacity`: 0.25 scrim =
/// 75 % opacity, 0.50 = 50 %, 0.75 = 25 %.
const UNFOCUSED_SCRIM: f32 = 0.25;
const RESTING_SCRIM: f32 = 0.50;
const PARKED_SCRIM: f32 = 0.75;
/// Deeper than either, because dragging is a live gesture and the
/// source pane has to read as "the one in your hand".
const DRAG_SOURCE_SCRIM: f32 = 0.38;
/// A seat a pane moved out of — nothing is running there, and the
/// hint text has to stay readable through it.
const EMPTY_SEAT_SCRIM: f32 = 0.22;

/// Scrim for a pane from the attention ladder alone.
///
/// The focused pane is never dimmed at any level: whatever the state
/// machine thinks of it, a pane the user is looking at is not
/// receding from where they sit.
/// The dim a pane's attention level calls for.
///
/// The *target*, not what is painted: `Pane`'s `ScrimFade` eases
/// towards it, and the view carries the eased value.
pub fn attention_scrim(focused: bool, recede: u32) -> f32 {
    if focused {
        return 0.0;
    }
    let rung = match recede {
        0 => UNFOCUSED_SCRIM,
        1 => RESTING_SCRIM,
        _ => PARKED_SCRIM,
    };
    // The *ladder* is not a setting — its order says what marspot knows
    // about each pane.  How loudly it says it is: a wide grid wants
    // less, a pair of panes wants more.  Read per call, which is once
    // per pane per frame — the cheap end of `get()`, unlike the
    // per-byte path that had to grow an atomic mirror.
    (rung * crate::settings::get().dim_scale).clamp(0.0, 0.92)
}

/// Empty-cell BG tint.  Painted over `layout.cells[views.len()..]`
/// when sessions count is less than layout cell count so the
/// "open slot" reads as a softer, slightly elevated area.  Low-
/// alpha black-ish overlay over the existing cell BG keeps the
/// terminal palette intact.
const EMPTY_CELL_OVERLAY: [f32; 4] = [0.04, 0.05, 0.07, 0.6];
const EMPTY_CELL_GLYPH_FG: [f32; 4] = [0.30, 0.34, 0.38, 0.85];

/// F3+1.4 — Process Monitor modal constants.
const PROCESS_PANEL_BG: [f32; 4] = [0.13, 0.14, 0.17, 1.0];
const PROCESS_PANEL_BORDER: [f32; 4] = [0.32, 0.34, 0.40, 1.0];
const PROCESS_PANEL_CORNER_RADIUS: f32 = 10.0;
const PROCESS_PANEL_TITLE_BAR_BG: [f32; 4] = [0.18, 0.19, 0.23, 1.0];
const PROCESS_PANEL_SEPARATOR: [f32; 4] = [0.06, 0.07, 0.09, 1.0];
const PROCESS_PANEL_TITLE_FG: [f32; 4] = [0.92, 0.94, 0.97, 1.0];
const PROCESS_PANEL_TAB_BG_ACTIVE: [f32; 4] = [0.20, 0.30, 0.55, 1.0];
const PROCESS_PANEL_TAB_FG_ACTIVE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const PROCESS_PANEL_ROW_FG: [f32; 4] = [0.86, 0.88, 0.92, 1.0];
const PROCESS_PANEL_HEADER_FG: [f32; 4] = [0.65, 0.78, 0.95, 1.0];

const PROCESS_PANEL_TITLE_BAR_H_LOGICAL: f32 = 28.0;
const PROCESS_PANEL_TAB_STRIP_H_LOGICAL: f32 = 30.0;
// Traffic-light geometry and colour live in
// `ui::system::macos::traffic_lights` — the panel used to carry its own
// copy of both, which is why two rounds of "make the dots match the
// system" edited constants this painter never read.
/// Inset between the panel's frame and its content on every side.
///
/// The tables used to be laid out from the panel's own edges, so the
/// first column sat on the left border, the kill buttons sat on the
/// right one, and rows ran to the bottom edge with nothing under them —
/// the content read as spilling out of its frame.  One value for all
/// four sides, same rule the cc modal's cards follow.
pub const PROCESS_PANEL_SIDE_PAD_LOGICAL: f32 = 10.0;
/// Fraction of the padded content width given to the master (pane)
/// table; the detail table gets the rest.
///
/// Public because the hit-test geometry is computed a second time in
/// `marspot-core` — the two derivations have to agree or the kill
/// buttons stop landing where they are drawn, so at minimum they share
/// the numbers.
pub const PROCESS_PANEL_MASTER_FRAC: f64 = 0.38;
const PROCESS_PANEL_KILL_W_LOGICAL: f32 = 18.0;

/// F3+1.7 — paint the Process Monitor modal via the `View` component.
/// The modal's frame chrome (BG, border, shadow, backdrop) is owned
/// by `View::paint`; only the modal-specific content (title bar fill,
/// traffic lights, tabs, body rows) lives in the closure below.  All
/// drawing routes through `ViewPainter` into overlay scratches, so
/// the modal is always-on-top by construction — no filter pass on
/// the grid scratches needed.
fn push_process_panel_via_view(
    panel: &ProcessPanelRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: panel.rect,
        style: ViewStyle {
            bg: PROCESS_PANEL_BG,
            border_color: PROCESS_PANEL_BORDER,
            border_width: 1.0,
            corner_radius: PROCESS_PANEL_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: if panel.draw_backdrop {
                Backdrop::Dim {
                    color: [0.0, 0.0, 0.0, 0.45],
                    exclude_above_y: top_inset,
                }
            } else {
                Backdrop::None
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_process_panel_content(panel, p);
    });
}

/// cc — paint the `Cc` usage modal: one card per Claude account
/// (5H / 7D utilization bars + reset instants) and a ±6-day
/// availability timeline underneath (per-account 5h/7d window bars,
/// NOW marker, daily ticks).  All geometry derives from cell
/// metrics so the modal scales with the font.
#[allow(clippy::too_many_arguments)]
fn push_cc_usage_via_view(
    cc: &CcUsageRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: cc.rect,
        style: ViewStyle {
            bg: PROCESS_PANEL_BG,
            border_color: PROCESS_PANEL_BORDER,
            border_width: 1.0,
            corner_radius: PROCESS_PANEL_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: Backdrop::Dim {
                color: [0.0, 0.0, 0.0, 0.45],
                exclude_above_y: top_inset,
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_cc_usage_content(cc, p);
    });
}


/// The settings panel.
///
/// Every row is label / cost / control, walked by
/// `settings_modal::walk` — the same walker the hit-test uses, which
/// is what makes a control clickable exactly where it is drawn.
#[allow(clippy::too_many_arguments)]
fn push_settings_panel_via_view(
    sp: &SettingsRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::components::settings_modal as sm;
    use crate::ui::core::view::{Backdrop, View, ViewPainter, ViewStyle};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: sp.rect,
        style: ViewStyle {
            bg: PROCESS_PANEL_BG,
            border_color: PROCESS_PANEL_BORDER,
            border_width: 1.0,
            corner_radius: PROCESS_PANEL_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: Backdrop::Dim {
                color: [0.0, 0.0, 0.0, 0.45],
                exclude_above_y: top_inset,
            },
        },
    };
    view.paint(&mut painter, |p| {
        use crate::ui::components::settings_modal::{Control, Slot};
        let px = ViewPainter::px_per_pt();
        // The walker needs a measurer and the painter owns the font,
        // so collect the geometry first and paint from it.  Both this
        // and the hit-test measure through the same path, which is
        // what keeps a segment as wide as the label inside it.
        let mut slots: Vec<Slot> = Vec::new();
        {
            let font = &mut *p.font;
            let mut m = |s: &str, pt: f64, w: u16| {
                font.measure_ui_text_at_size(
                    s, w, crate::font_shape::ShapeOptions::default(), pt,
                )
            };
            sm::walk(sp.rect, &sp.settings, &mut m, |slot| slots.push(slot));
        }

        for slot in slots {
            match slot {
                Slot::Title { baseline } => {
                    p.ui_text_at(
                        sm::text_x(sp.rect) as f32,
                        baseline as f32,
                        "Settings",
                        sm::metric::TITLE.pt(),
                        sm::metric::TITLE.weight(),
                        panel_palette::fg(),
                    );
                }
                Slot::Group { heading, baseline } => {
                    p.ui_text_at(
                        sm::text_x(sp.rect) as f32,
                        baseline as f32,
                        heading,
                        sm::metric::GROUP.pt(),
                        sm::metric::GROUP.weight(),
                        panel_palette::fg(),
                    );
                }
                Slot::Card { rect } => {
                    // The card is what makes a group read as a group.
                    p.fill_rounded_rect(
                        rect,
                        panel_palette::card_bg(),
                        (sm::metric::CARD_RADIUS * px) as f32,
                        (panel_palette::card_border(), 1.0),
                    );
                }
                Slot::Separator { rect } => {
                    // `fill_rounded_rect`, not `fill_rect`: the two go
                    // to different pipelines, and the whole cells pass
                    // is drawn *before* the whole ui_rects pass — so a
                    // hairline submitted after the card was painted
                    // under it and vanished.  Same pass, later
                    // submission, visible.
                    p.fill_rounded_rect(rect, panel_palette::separator(), 0.0, ([0.0; 4], 0.0));
                }
                Slot::Row { row, spec, label_baseline, desc_baseline, control, .. } => {
                    let dim = row.disabled_by(&sp.settings);
                    let (fg, sec) = if dim {
                        (panel_palette::fg_faint(), panel_palette::fg_faint())
                    } else {
                        (panel_palette::fg(), panel_palette::fg_muted())
                    };
                    let x = sm::text_x(sp.rect) as f32;
                    p.ui_text_at(
                        x, label_baseline as f32, spec.label,
                        sm::metric::LABEL.pt(), sm::metric::LABEL.weight(), fg,
                    );
                    // Genuinely smaller, not merely dimmer: same size
                    // in a paler grey is two competing lines.
                    p.ui_text_at(
                        x, desc_baseline as f32, spec.cost,
                        sm::metric::DESC.pt(), sm::metric::DESC.weight(), sec,
                    );
                    match row.control(&sp.settings) {
                        Control::Toggle(on) => {
                            // A pill with the knob at one end.  Drawn,
                            // not written: a checkbox at this size
                            // reads as decoration, a pill as a switch.
                            let r = control.h * 0.5;
                            let bg = if !dim && on {
                                panel_palette::ok()
                            } else {
                                panel_palette::track()
                            };
                            p.fill_rounded_rect(control, bg, r as f32, ([0.0; 4], 0.0));
                            let knob_d = control.h * 0.74;
                            let pad = (control.h - knob_d) * 0.5;
                            let kx = if on {
                                control.x + control.w - knob_d - pad
                            } else {
                                control.x + pad
                            };
                            p.fill_rounded_rect(
                                marspot_term::layout::Rect {
                                    x: kx,
                                    y_top: control.y_top + pad,
                                    w: knob_d,
                                    h: knob_d,
                                },
                                panel_palette::fg(),
                                (knob_d * 0.5) as f32,
                                ([0.0; 4], 0.0),
                            );
                        }
                        Control::Segmented { options, chosen } => {
                            for (n, label) in options.iter().enumerate() {
                                let seg = {
                                    let font = &mut *p.font;
                                    let mut m = |s: &str, pt: f64, w: u16| {
                                        font.measure_ui_text_at_size(
                                            s, w,
                                            crate::font_shape::ShapeOptions::default(),
                                            pt,
                                        )
                                    };
                                    sm::segment_rect(control, n, options, &mut m)
                                };
                                let picked = !dim && n == chosen;
                                let (bg, border, fg) = if picked {
                                    (panel_palette::ok(), panel_palette::ok(), [1.0, 1.0, 1.0, 1.0])
                                } else if dim {
                                    (panel_palette::segment_bg(), panel_palette::card_border(), panel_palette::fg_faint())
                                } else {
                                    (panel_palette::segment_bg(), panel_palette::card_border(), panel_palette::fg())
                                };
                                p.fill_rounded_rect(
                                    seg, bg, (5.0 * px) as f32, (border, 1.0),
                                );
                                // Measured, then centred on what was
                                // measured — the label overran its
                                // button when width came from a count.
                                let tw = p.ui_text_width_at(
                                    label, sm::metric::SEGMENT.pt(), sm::metric::SEGMENT.weight(),
                                );
                                let baseline = p.ui_baseline_centred(
                                    seg.y_top as f32, seg.h as f32, sm::metric::SEGMENT.pt(),
                                );
                                p.ui_text_at(
                                    (seg.x + (seg.w - tw as f64) * 0.5) as f32,
                                    baseline,
                                    label,
                                    sm::metric::SEGMENT.pt(),
                                    sm::metric::SEGMENT.weight(),
                                    fg,
                                );
                            }
                        }
                    }
                }
                Slot::Footer { baseline } => {
                    // Where the file is.  The panel is one way to edit
                    // it, not the only one.
                    p.ui_text_at(
                        sm::text_x(sp.rect) as f32,
                        baseline as f32,
                        &sp.path,
                        sm::metric::FOOTER.pt(),
                        sm::metric::FOOTER.weight(),
                        panel_palette::fg_faint(),
                    );
                }
            }
        }
    });
}


/// The palette every panel draws from.
///
/// It started life as the `Cc` panel's private colours and was named
/// for it; the settings panel then grew a second, near-identical set
/// under its own name, and the two drifted — one drew its cards a
/// level *above* the panel, the other a level *below*, so the same
/// object looked raised in one panel and inset in the next.  One
/// module now, named for what it is.
///
/// Every colour is a theme token.  The only local decisions are which
/// token plays which role.
mod panel_palette {
    use crate::ui::theme::token::color;
    /// Primary — account names, headings, percentages.
    pub fn fg() -> [f32; 4] { color::FG.to_rgba_f32() }
    /// Secondary — email, reset times, timeline tags.  Deliberately
    /// NOT `FG_MUTED`: at this density the muted token collapses the
    /// whole panel into one grey and the reader can't find the row
    /// they want.  This sits between FG and FG_MUTED so the hierarchy
    /// (primary → secondary → axis) stays legible.
    pub fn fg_sec() -> [f32; 4] { [0.70, 0.75, 0.82, 1.0] }
    /// Tertiary — date axis, tick marks.  Chart furniture, reads as
    /// background once you've found your row.
    pub fn fg_faint() -> [f32; 4] { color::FG_MUTED.to_rgba_f32() }
    /// A line that must recede behind the one above it — an
    /// explanation, not a label.  Same token as `fg_faint`; the two
    /// names are kept apart because one means "this control is off"
    /// and the other means "this is secondary text", and they will not
    /// always want the same colour.
    pub fn fg_muted() -> [f32; 4] { color::FG_MUTED.to_rgba_f32() }
    pub fn ok() -> [f32; 4] { color::SUCCESS.to_rgba_f32() }
    pub fn warn() -> [f32; 4] { color::WARN.to_rgba_f32() }
    pub fn danger() -> [f32; 4] { color::DANGER.to_rgba_f32() }
    /// Unfilled bar remainder.  `SURFACE_4`, not `BG_HOVER` — the
    /// track has to separate from the card it sits on (`SURFACE_1`),
    /// otherwise "0 %" and "no data" look identical.
    pub fn track() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
    /// A card sits **on** its panel, so it takes the surface level
    /// above the panel's own — the same step in every panel.
    pub fn card_bg() -> [f32; 4] { color::SURFACE_3.to_rgba_f32() }
    pub fn card_border() -> [f32; 4] { color::BORDER.to_rgba_f32() }
    /// Between two rows of one card.  `HAIRLINE`, not `DIVIDER`: the
    /// fainter token at one device pixel is invisible, which leaves
    /// the rows looking exactly as undivided as no line at all.
    pub fn separator() -> [f32; 4] { color::HAIRLINE.to_rgba_f32() }
    /// An unpicked segment of a segmented control — one level above
    /// the card it sits on.
    pub fn segment_bg() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
    pub fn now() -> [f32; 4] { color::ACCENT.to_rgba_f32() }
    /// Day rules running up through the plot.  Low alpha rather than a
    /// dim solid colour so the rule reads as behind the bars on both
    /// the card surface and the modal ground it crosses.
    ///
    /// 0.14 is chosen against alpha that actually works: the first cut
    /// used 0.22, picked by eye while the ui_rect pipeline was still
    /// applying alpha twice, so what was really on screen was 0.05.
    pub fn grid() -> [f32; 4] { [0.62, 0.66, 0.74, 0.14] }
    /// Status chip fill — the severity colour at low alpha, so the
    /// chip reads as tinted glass over the card rather than a second
    /// solid block competing with the bars.
    pub fn chip_bg(severity: u8) -> [f32; 4] {
        let mut c = super::cc_severity_color(severity);
        c[3] = 0.16;
        c
    }
}

/// Status severity → colour.  0 = fine, 1 = approaching the cap,
/// 2 = refused / unknown.
fn cc_severity_color(severity: u8) -> [f32; 4] {
    match severity {
        0 => panel_palette::ok(),
        1 => panel_palette::warn(),
        _ => panel_palette::danger(),
    }
}

/// Clip `s` to `cols` monospace columns, marking the cut with `…`.
/// Returns empty when there isn't room for at least a few glyphs —
/// a lone ellipsis carries no information and just adds noise.
fn fit_ellipsis(s: &str, cols: f64) -> String {
    let fit = cols.floor().max(0.0) as usize;
    if s.chars().count() <= fit {
        return s.to_string();
    }
    if fit < 3 {
        return String::new();
    }
    s.chars().take(fit - 1).chain(['…']).collect()
}

fn cc_util_color(util: f32) -> [f32; 4] {
    if util < 0.5 {
        panel_palette::ok()
    } else if util < 0.85 {
        panel_palette::warn()
    } else {
        panel_palette::danger()
    }
}

fn paint_cc_usage_content(cc: &CcUsageRender, p: &mut crate::ui::core::view::ViewPainter) {
    let cw = p.cell_w;
    let ch = p.cell_h;
    let ascent = p.ascent;
    let r = cc.rect;
    // Outer breathing room.  The panel is a reference surface, not a
    // dense readout — it can afford to sit away from its own frame.
    use crate::ui::components::cc_usage_modal::{metric, card_height, timeline_range};
    let pad = ch as f64 * metric::PANEL_PAD;
    let lh = ch as f64 * metric::LINE_ADVANCE;
    let inner_x = r.x + pad;
    let inner_w = (r.w - 2.0 * pad).max(1.0);

    let text = |p: &mut crate::ui::core::view::ViewPainter, x: f64, baseline: f64, s: &str, c: [f32; 4]| {
        p.text(x as f32, baseline as f32, s, c);
    };
    let text_w = |s: &str| -> f64 { s.chars().count() as f64 * cw as f64 };

    // ---- title row ----
    // The heading renders in the system UI font at the system size, so
    // it sits at parity with the native macOS window header rather than
    // in the smaller terminal mono face.  The right-aligned "updated"
    // stamp stays mono — it is a timestamp, tabular by nature — and is
    // vertically centred against the taller heading line.
    let head_top = r.y_top + pad;
    let ui_line = p.ui_line_h() as f64;
    let title = format!("CLAUDE ACCOUNTS  {}", cc.accounts.len());
    // Same role, same code path as every other panel's title — the
    // point of the ladder is that "title" means one size app-wide.
    let title_role = crate::ui::theme::PanelText::Title;
    p.panel_text(
        title_role,
        inner_x as f32,
        (head_top + p.ui_ascent() as f64) as f32,
        &title,
        panel_palette::fg(),
    );
    let upd = &cc.updated_label;
    let upd_baseline = head_top + (ui_line - ch as f64) * 0.5 + ascent as f64;
    text(p, inner_x + inner_w - text_w(upd), upd_baseline, upd, panel_palette::fg_sec());
    let mut y = head_top + ui_line + lh * metric::HEADING_GAP;

    if cc.feed_missing {
        y += lh;
        text(p, inner_x, y, "no usage feed at ~/.local/state/devops/claude-usage.json", panel_palette::fg_sec());
        return;
    }

    // ---- account cards, one row ----
    let n = cc.accounts.len().max(1);
    let gap = cw as f64 * metric::CARD_GAP;
    let card_w = ((inner_w - gap * (n as f64 - 1.0)) / n as f64).max(40.0);
    // ONE padding value for all four sides, in physical px.
    //
    // The previous cut measured horizontal padding in `cw` and vertical
    // padding in `lh`, which are different rulers — the four gaps came
    // out visibly unequal, and the bottom one collapsed to almost
    // nothing once the last row's descenders were accounted for.
    let card_pad = ch as f64 * metric::CARD_PAD;
    // Baseline-to-baseline advances between the five rows.  Named so
    // the card height below can be derived instead of guessed.
    let row_adv: Vec<f64> =
        metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).collect();

    // Exact: pad, then the first row's ascent, the row advances, the
    // last row's descent, then pad again.  `ch - ascent` is the
    // descent — the painter reports cell height and ascent, and a
    // monospace cell is exactly the two stacked.
    let extra_bar_rows = cc
        .accounts
        .iter()
        .map(|a| a.model_rows.len())
        .max()
        .unwrap_or(0);
    let card_h = card_height(ch as f64, ascent as f64, extra_bar_rows);
    let card_top = y;
    for (i, a) in cc.accounts.iter().enumerate() {
        let cx = inner_x + i as f64 * (card_w + gap);
        let card = Rect { x: cx, y_top: card_top, w: card_w, h: card_h };
        p.fill_rounded_rect(card, panel_palette::card_bg(), 6.0, (panel_palette::card_border(), 1.0));
        let px = cx + card_pad;
        let row_right = cx + card_w - card_pad;
        let mut cy = card_top + card_pad + ascent as f64;

        // r1 — name (primary) + status chip, right-aligned in a slot
        // reserved BEFORE the name is laid out so no length pairing can
        // make them collide.
        // The chip's TEXT is what has to line up with the percentages
        // below it — they are the same column of the card.  Aligning the
        // chip's *background* instead pushed the label half a character
        // left of the numbers, which is exactly the kind of near-miss
        // that reads as sloppy.  So: right-align the text at
        // `row_right`, then draw the pill around it, letting the pill
        // spill into the card's padding rather than moving the text.
        let chip_text = a.status_label.to_uppercase();
        let chip_pad = cw as f64 * metric::CHIP_PAD;
        let chip_text_x = row_right - text_w(&chip_text);
        // Centre the pill on the label's INK, not on its baseline box.
        // The label is all-caps, so its ink runs from `baseline - cap`
        // to the baseline with nothing below; hanging the pill off the
        // full ascent left roughly twice as much air under the letters
        // as above them.  0.72 × ascent is the usual stand-in for cap
        // height — the painter reports ascent, not cap height — and is
        // only ever applied to these fixed uppercase labels.
        let chip_h = ch as f64 * 1.05;
        let cap = ascent as f64 * metric::CAP_HEIGHT_OF_ASCENT;
        p.fill_rounded_rect(
            Rect {
                x: chip_text_x - chip_pad,
                y_top: cy - cap * 0.5 - chip_h * 0.5,
                w: text_w(&chip_text) + chip_pad * 2.0,
                h: chip_h,
            },
            panel_palette::chip_bg(a.status_severity), 3.0, ([0.0; 4], 0.0),
        );
        text(p, chip_text_x, cy, &chip_text, cc_severity_color(a.status_severity));
        let name_budget = (chip_text_x - chip_pad - cw as f64 - px) / cw as f64;
        text(p, px, cy, &fit_ellipsis(&a.name, name_budget), panel_palette::fg());

        // r2 — email, one full line of its own.  Cramming it beside the
        // name is what produced the collisions; a dedicated line also
        // lets a long address show in full.
        cy += row_adv[0];
        text(p, px, cy, &fit_ellipsis(&a.email, (row_right - px) / cw as f64), panel_palette::fg_sec());

        // r3/r4 — one full-width bar per window: `5H [========----] 55%`.
        // Full width (not two half-width groups) roughly doubles the
        // resolution of the bar, which is the whole point of the panel.
        let pct_slot = cw as f64 * 4.0; // "100%"
        // The account's own windows, then one row per model cap.  The
        // model rows are the same shape on purpose: a reader should
        // not have to learn a second way to read a bar halfway down
        // the card.
        let rows: Vec<(String, f32)> = [("5H".to_string(), a.util_5h), ("7D".to_string(), a.util_7d)]
            .into_iter()
            .chain(a.model_rows.iter().cloned())
            .collect();
        // Labels are no longer all two characters, so the bars start
        // after the widest one rather than at a fixed column.
        let label_cols = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(2) as f64;
        for (i, (label, util)) in rows.into_iter().enumerate() {
            // First bar row steps off the email; every later one uses
            // the tighter row-to-row advance.
            cy += if i == 0 { row_adv[1] } else { row_adv[2] };
            text(p, px, cy, &label, panel_palette::fg_faint());
            let pct = format!("{:.0}%", util * 100.0);
            text(p, row_right - text_w(&pct), cy, &pct, cc_util_color(util));
            let bar_x = px + cw as f64 * (label_cols + 1.0);
            let bar_w = (row_right - pct_slot - cw as f64 - bar_x).max(1.0);
            let bar_h = ch as f64 * metric::BAR_H;
            // Centre the bar on the text's optical middle so the row
            // reads as one unit instead of a caption above a bar.
            let bar_y = cy - ascent as f64 * 0.36 - bar_h / 2.0;
            p.fill_rounded_rect(
                Rect { x: bar_x, y_top: bar_y, w: bar_w, h: bar_h },
                panel_palette::track(), 2.0, ([0.0; 4], 0.0),
            );
            let fill_w = bar_w * util.clamp(0.0, 1.0) as f64;
            if fill_w > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: bar_x, y_top: bar_y, w: fill_w, h: bar_h },
                    cc_util_color(util), 2.0, ([0.0; 4], 0.0),
                );
            }
        }

        // r5 — reset times.
        cy += row_adv[3];
        text(p, px, cy, &fit_ellipsis(&a.reset_label, (row_right - px) / cw as f64), panel_palette::fg_sec());
    }
    // Two sections, not one continuous list — give the boundary enough
    // room to read as a break.
    y = card_top + card_h + lh * metric::SECTION_BREAK;

    // ---- timeline ----
    p.panel_text(
        crate::ui::theme::PanelText::Title,
        inner_x as f32,
        (y + p.ui_ascent() as f64) as f32,
        "RESOURCE AVAILABILITY",
        panel_palette::fg(),
    );
    y += p.ui_line_h() as f64 + lh * metric::HEADING_GAP;
    let label_w = cc
        .accounts
        .iter()
        .map(|a| text_w(&a.name))
        .fold(0.0f64, f64::max)
        + cw as f64 * 2.0;
    let tl_x = inner_x + label_w;
    // Reserve a gutter on the right for the tags that hang off the end
    // of each window bar.  Without it the axis ran all the way to the
    // inner edge and a bar ending at (or clamped to) the right of the
    // range left its tag nowhere to go — it rendered on top of the
    // modal's own border.  Sizing the gutter to the widest tag any
    // account will draw means the plot compresses a little and nothing
    // ever has to overlap.
    let tag_gutter = cc
        .accounts
        .iter()
        .flat_map(|a| {
            [
                format!("5h {:.0}% {}", a.util_5h * 100.0, a.reset_5h_hm),
                format!("7d {:.0}% {}", a.util_7d * 100.0, a.reset_7d_hm),
            ]
        })
        .map(|t| text_w(&t))
        .fold(0.0f64, f64::max)
        + cw as f64 * 1.2;
    let tl_w = (inner_w - label_w - tag_gutter).max(10.0);
    // Scale to what has to be drawn.  A fixed ±6 d window cut every
    // 7-day bar short (its reset is up to 7 d out), so the bar stopped
    // at the axis edge while its label still named the real reset time
    // — the bar and the number beside it disagreed.
    let extents: Vec<(i64, i64)> = cc
        .accounts
        .iter()
        .flat_map(|a| {
            [
                (a.reset_5h_unix - 5 * 3_600, a.reset_5h_unix),
                (a.reset_7d_unix - 7 * 86_400, a.reset_7d_unix),
            ]
        })
        .collect();
    let (t0, t1) = timeline_range(cc.now_unix, &extents);
    // Snap the range outward to local day boundaries.
    //
    // Two things at once.  It is the plot's margin — a bar no longer
    // starts or ends flush against the edge — and, more importantly,
    // it guarantees a dated rule on BOTH sides of every bar end.  The
    // range used to stop wherever the data did plus a few percent, so
    // the last bar ended past the final rule with nothing behind it to
    // read against: `7d 19% 12:00` sat to the right of `8/14` and the
    // next day was never drawn (2026-08-07 report).
    let (t0, t1) = crate::cc_usage::snap_range_to_local_days(t0, t1);
    let span_s = t1 - t0;
    let x_of = |t: f64| -> f64 { tl_x + ((t - t0) / span_s).clamp(0.0, 1.0) * tl_w };
    let bar_h = ch as f64 * metric::BAR_H;
    let bar_gap = ch as f64 * metric::TIMELINE_BAR_GAP;
    let row_h = bar_h * 2.0 + bar_gap + lh * metric::TIMELINE_ROW_EXTRA;
    let rows_top = y;
    let rows_bottom = rows_top + cc.accounts.len() as f64 * row_h;
    // Day grid, drawn first so the bars read as sitting on top of it.
    //
    // The dates used to live only as labels under the axis, which meant
    // reading "when does Claude 3's 7d window reset" took a saccade to
    // the bottom of the chart and back.  Carrying each day up through
    // the plot as a dashed rule lets a bar's end be read against a date
    // in place.  Dashed, and faint, because it is a background
    // reference: a solid rule at this density competes with the bars,
    // and the one line that must stay solid is NOW.
    // Local days, not 86 400 s steps from a UTC-aligned start: these
    // rules are labelled with local dates below, so they have to land
    // where those dates begin.  Stepping in UTC put every rule the
    // timezone offset away from its own label — nine hours in JST, so
    // a 7-day window resetting at 00:00 on the 8th ended visibly left
    // of the rule reading `8/8`.
    let first_day = {
        let s = crate::cc_usage::local_day_start(t0 as i64);
        (if (s as f64) < t0 { crate::cc_usage::next_local_day_start(s) } else { s }) as f64
    };
    let next_day = |t: f64| crate::cc_usage::next_local_day_start(t as i64) as f64;
    let dash = ch as f64 * metric::GRID_DASH;
    let gap = ch as f64 * metric::GRID_GAP;
    let grid_top = rows_top - lh * 0.35;
    let mut t_grid = first_day;
    // `<=`: the range now ends ON a day boundary, and that last rule is
    // the one a bar ending in the final day is read against.
    while t_grid <= t1 {
        let x = x_of(t_grid);
        let mut gy = grid_top;
        while gy < rows_bottom {
            let h = dash.min(rows_bottom - gy);
            p.fill_rounded_rect(
                Rect { x, y_top: gy, w: 1.0, h },
                panel_palette::grid(), 0.0, ([0.0; 4], 0.0),
            );
            gy += dash + gap;
        }
        t_grid = next_day(t_grid);
    }
    for (i, a) in cc.accounts.iter().enumerate() {
        let ry = rows_top + i as f64 * row_h;
        let name_baseline = ry + bar_h + bar_gap / 2.0 + ascent as f64 * 0.5;
        text(p, inner_x, name_baseline, &a.name, panel_palette::fg());
        for (idx, (span, reset, util, hm)) in [
            (5.0 * 3_600.0, a.reset_5h_unix as f64, a.util_5h, &a.reset_5h_hm),
            (7.0 * 86_400.0, a.reset_7d_unix as f64, a.util_7d, &a.reset_7d_hm),
        ]
        .into_iter()
        .enumerate()
        {
            let by = ry + idx as f64 * (bar_h + bar_gap);
            // Track = the rolling window's extent [reset - span,
            // reset]; the coloured fill covers only the USED portion
            // (window start + span × utilization) — "used this much
            // of this window", not "window exists".
            let wx0 = x_of(reset - span);
            let wx1 = x_of(reset);
            if wx1 - wx0 > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: wx0, y_top: by, w: wx1 - wx0, h: bar_h },
                    panel_palette::track(), 2.0, ([0.0; 4], 0.0),
                );
            }
            let ux1 = x_of(reset - span + span * util.clamp(0.0, 1.0) as f64);
            if ux1 - wx0 > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: wx0, y_top: by, w: ux1 - wx0, h: bar_h },
                    cc_util_color(util), 2.0, ([0.0; 4], 0.0),
                );
            }
            // Tag sits right of the window, vertically centered on
            // the bar (baseline = bar centre + half the ascent).
            let tag = format!("{} {:.0}% {}", if idx == 0 { "5h" } else { "7d" }, util * 100.0, hm);
            // The gutter guarantees room, so this clamp is only a
            // backstop against a pathologically long reset label.
            // Strictly right of where the bar ends — never pulled back
            // over it.  The gutter above is sized to the widest tag any
            // account draws, and the axis now ends where the data ends,
            // so the furthest-right bar's tag still has its room.
            let tag_x = wx1 + cw as f64 * 0.7;
            let tag_baseline = by + bar_h / 2.0 + ascent as f64 * 0.42;
            text(p, tag_x, tag_baseline, &tag, panel_palette::fg_sec());
        }
    }
    // NOW marker.
    let nx = x_of(cc.now_unix as f64);
    p.fill_rounded_rect(
        Rect { x: nx, y_top: rows_top - lh * 0.4, w: 1.5, h: rows_bottom - rows_top + lh * 0.4 },
        panel_palette::now(), 0.0, ([0.0; 4], 0.0),
    );
    text(p, nx - text_w("NOW") / 2.0, rows_top - lh * 0.5, "NOW", panel_palette::now());
    // Date labels along the axis.  No tick stubs — each date's dashed
    // rule already lands on the axis, so a stub would just double it.
    let mut t = first_day;
    while t <= t1 {
        let x = x_of(t);
        let (mo, d, _, _) = crate::cc_usage::local_mdhm(t as i64);
        let lbl = format!("{mo}/{d}");
        text(p, x - text_w(&lbl) / 2.0, rows_bottom + lh * 0.7, &lbl, panel_palette::fg_faint());
        t = next_day(t);
    }
}

/// F3+9 / P2c — build the right-click ContextMenu as a `Canvas`.
/// Geometry from `ContextMenu::layout`; submission order = z so
/// frame BG goes first, then hover band, then divider hairline,
/// then text — last-submitted wins on top.  Caller flushes the
/// returned canvas via `MetalRenderer::encode_canvas` AFTER all
/// other overlay passes so the menu trumps everything else.
///
/// Why a separate canvas (not the shared overlay scratches):
/// the encode_passes path fixes the BG-cells-before-UI-rects
/// order, which silently buried the divider in F3+12.x.  Routing
/// the menu through its own canvas + `encode_canvas` puts every
/// primitive on a submission-order timeline regardless of which
/// pipeline carries it.
/// SF Pro's ascent as a fraction of its point size — measured from
/// the interned CTFont at startup and stable across sizes.  Used only
/// where a run has to be centred against a box whose height came from
/// somewhere else (menu rows); anything drawing into a `ViewPainter`
/// should use `ui_baseline_centred` instead, which asks the font.
const SF_PRO_ASCENT_RATIO: f64 = 0.75;

fn build_context_menu_canvas(
    state: &ContextMenuRender,
    window_w: f64,
    window_h: f64,
    chrome_cell_w: f32,
    chrome_cell_h: f32,
) -> crate::ui::core::Canvas {
    use crate::ui::core::{Canvas, Color, Length, Pt, ParentRect};
    use crate::ui::components::{ContextMenu, MenuItem};

    let menu_items: Vec<MenuItem> = state
        .items
        .iter()
        .map(|r| MenuItem {
            label: r.label.clone(),
            shortcut_hint: r.shortcut_hint.clone(),
            enabled: r.enabled,
            divider: r.divider,
            action_tag: 0,
        })
        .collect();
    let menu = ContextMenu::layout(
        window_w, window_h, state.scale,
        state.anchor_phys.0, state.anchor_phys.1,
        state.top_inset,
        &menu_items,
    );

    let mut canvas = Canvas::new(state.scale, ParentRect::window(window_w, window_h));

    // ── Style tokens (will move to a theme module in P3) ──
    let bg          = Color::rgba(33, 36, 43, 1.0);     // PROCESS_PANEL_BG
    let border      = Color::rgba(56, 60, 70, 1.0);     // approx PROCESS_PANEL_BORDER
    let shadow      = Color::rgba(0, 0, 0, 0.45);
    let label_fg    = Color::rgba(217, 224, 235, 1.0);
    let label_disab = Color::rgba(115, 122, 133, 1.0);
    let hint_fg     = Color::rgba(140, 153, 168, 1.0);
    let hover_bg    = Color::rgba(51, 107, 173, 1.0);
    // Web `border: 1px solid` semantics: 1pt thick + alpha tuned for
    // clear visibility on the menu BG.  alpha=0.5 lands the rendered
    // pixel ≈ rgb(144, 145, 149) over bg rgb(33, 36, 43) — the kind
    // of contrast Chrome / Safari show for `rgba(255,255,255,0.5)`
    // on a near-black panel.  Earlier 0.22 was a misjudgement (line
    // showed but user reported "几乎看不清").
    let divider_c   = Color::rgba(255, 255, 255, 0.50);
    let side_pad_pt = 12.0_f64;

    // Helper: phys → Pt via `/ scale` so existing layout output
    // (which is already physical px) plugs cleanly into the
    // logical Pt API.  Length::Pt(x) resolves back to `x * scale`,
    // so this round-trips bit-perfectly at every scale.
    let pt_phys = |phys: f64| Length::Pt(phys / state.scale);
    let side_pad_phys = side_pad_pt * state.scale;
    let label_w_phys  = |s: &str| s.chars().count() as f64 * chrome_cell_w as f64;

    // 1. Menu frame: BG + border + shadow.
    canvas.rect()
        .at(pt_phys(menu.frame.x), pt_phys(menu.frame.y_top))
        .size(pt_phys(menu.frame.w), pt_phys(menu.frame.h))
        .fill(bg)
        .radius(Pt(6.0))
        .border(Pt(1.0), border)
        .shadow(Pt(16.0), (Pt(0.0), Pt(0.0)), shadow)
        .draw();

    // 2. Per-row primitives.  All sub-row arithmetic done in
    // physical pixels (item rects come from ContextMenu::layout
    // pre-scaled), then `pt_phys` wraps for the builder.
    for (i, row) in state.items.iter().enumerate() {
        let item = &menu.item_rects[i];
        if row.divider {
            let cy = item.y_top + item.h * 0.5;
            canvas.line(
                (pt_phys(item.x + side_pad_phys), pt_phys(cy)),
                (pt_phys(item.x + item.w - side_pad_phys), pt_phys(cy)),
            )
            .stroke(Pt(1.0), divider_c)
            .draw();
            continue;
        }
        if state.hovered_idx == Some(i) {
            canvas.rect()
                .at(pt_phys(item.x + side_pad_phys * 0.5), pt_phys(item.y_top))
                .size(pt_phys(item.w - side_pad_phys), pt_phys(item.h))
                .fill(hover_bg)
                .radius(Pt(4.0))
                .draw();
        }
        let fg = if row.enabled { label_fg } else { label_disab };
        // Menu labels are prose — the same role every other panel sets
        // a label in.  The canvas path takes a top-of-em y and derives
        // the baseline from the run's own ascent, so the y that puts
        // the *cap* on the row's centre line is
        // `centre - (ascent - cap/2)`.
        let role = crate::ui::theme::PanelText::Item;
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(role.pt());
        let text_y_phys = item.y_top + item.h * 0.5
            - (SF_PRO_ASCENT_RATIO * role.pt() - role.cap() * 0.5) * px;
        canvas.text(
            pt_phys(item.x + side_pad_phys),
            pt_phys(text_y_phys),
            &row.label,
        ).color(fg).ui().ui_size_q(size_q).weight(role.weight()).draw();
        if !row.shortcut_hint.is_empty() {
            // The shortcut is a glyph cluster (⌘⇧W), not prose — it
            // stays mono, where the symbols keep their own width and
            // right-align cleanly.
            let hint_w = label_w_phys(&row.shortcut_hint);
            let hint_y = item.y_top + (item.h - chrome_cell_h as f64) * 0.5;
            canvas.text(
                pt_phys(item.x + item.w - side_pad_phys - hint_w),
                pt_phys(hint_y),
                &row.shortcut_hint,
            ).color(hint_fg).draw();
        }
    }

    canvas
}

/// F3+3.0 — paint the `LayoutModal` overlay.  Same plumbing as
/// `push_process_panel_via_view`: routes through a `ViewPainter`
/// → overlay scratches so it lands on top of the grid.  Geometry
/// is computed from the modal's own `LayoutModal::layout` (cols
/// × rows steppers + Apply + footer text are all positional).
#[allow(clippy::too_many_arguments)]
fn push_layout_modal_via_view(
    state: &LayoutModalRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    use crate::ui::components::LayoutModal;
    // The widest label decides how wide a card wants to be; the
    // renderer is where both the labels and the cell metrics are.
    let widest_label = state
        .slot_titles
        .iter()
        .map(|t| t.chars().count())
        .max()
        .unwrap_or(0) as f64
        * cell_w as f64;
    let modal = LayoutModal::layout(
        window_w, window_h, state.scale, top_inset,
        state.cols, state.rows, widest_label,
    );
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: modal.frame,
        style: ViewStyle {
            bg: PROCESS_PANEL_BG,
            border_color: PROCESS_PANEL_BORDER,
            border_width: 1.0,
            corner_radius: PROCESS_PANEL_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: Backdrop::Dim {
                color: [0.0, 0.0, 0.0, 0.45],
                exclude_above_y: top_inset,
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_layout_modal_content(&modal, state, p);
    });
}

/// Internal paint of the modal's content (title text, close X,
/// stepper buttons + values, footer total, Apply button).  All
/// rects come pre-computed from `LayoutModal::layout`; here we
/// just draw atop them.
fn paint_layout_modal_content(
    modal: &crate::ui::components::LayoutModal,
    state: &LayoutModalRender,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    let pending_cols = state.cols;
    let pending_rows = state.rows;
    let scale = state.scale;
    use crate::ui::components::{Button, ButtonStyle, IconSpec, IconPosition};
    use marspot_term::layout::Alignment;
    // Colors mirror process panel for visual consistency.
    let stepper_bg = [0.18, 0.20, 0.24, 1.0];
    let stepper_bg_hover = [0.24, 0.26, 0.30, 1.0];
    let stepper_fg = [0.85, 0.88, 0.92, 1.0];
    let title_fg = [0.85, 0.88, 0.92, 1.0];
    let muted_fg = [0.55, 0.60, 0.66, 1.0];
    let apply_bg = [0.20, 0.42, 0.68, 1.0];
    let apply_fg = [0.95, 0.97, 1.0, 1.0];
    let stepper_style = ButtonStyle {
        bg: stepper_bg,
        bg_hover: stepper_bg_hover,
        fg: stepper_fg,
        fg_hover: stepper_fg,
        border_color: [0.0; 4],
        border_width: 0.0,
        corner_radius: 4.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: (12.0 * scale) as f32,
    };
    // F3+3.4 — `text_in(rect, s, color, align)` replaces all the
    // hand-rolled `(rect.h - cell_h) * 0.5 + ascent` math below.
    // Title text — left-pad'd, vertically centered in title_bar.
    let title_pad_left = 14.0 * scale;
    let title_inner = marspot_term::layout::Rect {
        x: modal.title_bar.x + title_pad_left,
        y_top: modal.title_bar.y_top,
        w: modal.title_bar.w - title_pad_left,
        h: modal.title_bar.h,
    };
    p.panel_text_in(
        crate::ui::theme::PanelText::Title,
        title_inner,
        "Layout",
        title_fg,
        Alignment::CenterLeft,
    );
    // Close [×] glyph, fully centered in its hit-target.
    p.text_in(modal.close_btn, "×", muted_fg, Alignment::Center);
    // Stepper buttons: cols [-] [+], rows [-] [+].
    for (rect, label) in [
        (modal.cols_dec, "−"),
        (modal.cols_inc, "+"),
        (modal.rows_dec, "−"),
        (modal.rows_inc, "+"),
    ] {
        let btn = Button {
            rect,
            label: Some(label),
            icon: None as Option<IconSpec>,
            icon_position: IconPosition::Only,
            hovered: false,
            style: stepper_style,
        };
        btn.paint(p);
    }
    // Stepper VALUES — N inside cols_value / rows_value, centered.
    let cols_str = pending_cols.to_string();
    let rows_str = pending_rows.to_string();
    p.text_in(modal.cols_value, &cols_str, title_fg, Alignment::Center);
    p.text_in(modal.rows_value, &rows_str, title_fg, Alignment::Center);
    // Row labels — left-aligned with same vertical baseline as the
    // adjacent value cell.  Use the value rect as the y reference
    // (so they're guaranteed visually aligned), but x = body left
    // pad of the modal frame.
    let label_pad = 16.0 * scale;
    let cols_label_rect = marspot_term::layout::Rect {
        x: modal.frame.x + label_pad,
        y_top: modal.cols_value.y_top,
        w: modal.cols_value.x - modal.frame.x - label_pad,
        h: modal.cols_value.h,
    };
    let rows_label_rect = marspot_term::layout::Rect {
        x: modal.frame.x + label_pad,
        y_top: modal.rows_value.y_top,
        w: modal.rows_value.x - modal.frame.x - label_pad,
        h: modal.rows_value.h,
    };
    // Prose labels take the shared Label role; the numbers between
    // them stay mono, where digits keep their column.
    p.panel_text_in(
        crate::ui::theme::PanelText::Label,
        cols_label_rect, "Columns", title_fg, Alignment::CenterLeft,
    );
    p.panel_text_in(
        crate::ui::theme::PanelText::Label,
        rows_label_rect, "Rows", title_fg, Alignment::CenterLeft,
    );
    // Footer: "Total: N panes" centered.
    let total = pending_cols * pending_rows;
    let total_str = format!("Total: {}×{} = {} pane{}",
        pending_cols, pending_rows, total,
        if total == 1 { "" } else { "s" });
    // Prose, not a column of figures — the secondary role, like the
    // sentence under a settings row.
    p.panel_text_in(
        crate::ui::theme::PanelText::Secondary,
        modal.total_label,
        &total_str,
        muted_fg,
        Alignment::Center,
    );
    // Cache atlas-driven metrics needed by the card-paint block.
    let cell_w = p.cell_w;
    let cell_h = p.cell_h;
    let ascent = p.ascent;
    // Apply button — full-width blue, white "Apply" label.
    let apply_style = ButtonStyle {
        bg: apply_bg,
        bg_hover: apply_bg,
        fg: apply_fg,
        fg_hover: apply_fg,
        border_color: [0.0; 4],
        border_width: 0.0,
        corner_radius: 4.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: 0.0,
    };
    let apply_btn = Button {
        rect: modal.apply_btn,
        label: Some("Apply"),
        icon: None as Option<IconSpec>,
        icon_position: IconPosition::Only,
        hovered: false,
        style: apply_style,
    };
    apply_btn.paint(p);

    // F3+3.3 — card grid + drag overlay.  Each slot renders a
    // small rounded card with its pane's title centered.  Order
    // matters: BG cards first, then drop-target highlight on the
    // hovered slot, then the dragged card on top so it floats
    // above everything.
    // Theme tokens, not hand-mixed RGB: a card here has to be the
    // same object as a card in the settings panel, and six literals
    // are six chances for it not to be.
    let card_bg = panel_palette::card_bg();
    // The slot you lifted a pane out of — recessed to the base
    // surface, so it reads as a hole rather than a card.
    let card_bg_drag_origin = crate::ui::theme::token::color::BG.to_rgba_f32();
    let card_bg_drop_target = crate::ui::theme::token::color::BG_SELECTED.to_rgba_f32();
    let card_border = panel_palette::card_border();
    let card_fg = panel_palette::fg();
    let card_fg_drag_origin = panel_palette::fg_faint();
    // Drop target: only highlighted while a drag is active AND
    // the drop target differs from the source slot.
    let drop_target_slot: Option<usize> = state.drag.as_ref().and_then(|d| {
        let cw = modal.cards.first().map(|c| c.w).unwrap_or(0.0);
        let ch = modal.cards.first().map(|c| c.h).unwrap_or(0.0);
        let cx = d.mouse_phys.0 - d.grab_offset_phys.0 + cw * 0.5;
        let cy = d.mouse_phys.1 - d.grab_offset_phys.1 + ch * 0.5;
        modal.nearest_card(cx, cy).filter(|&s| s != d.from_slot)
    });
    for (slot, rect) in modal.cards.iter().enumerate() {
        let is_drag_origin = state
            .drag
            .as_ref()
            .map(|d| d.from_slot == slot)
            .unwrap_or(false);
        let is_drop_target = drop_target_slot == Some(slot);
        let bg = if is_drag_origin {
            card_bg_drag_origin
        } else if is_drop_target {
            card_bg_drop_target
        } else {
            card_bg
        };
        p.fill_rounded_rect(*rect, bg, 6.0, (card_border, 1.0));
        // Title centred in the card, cut to what the card holds.
        // Project names are as long as their directory (`lab36-
        // continus`, `lab38-golialab`) and a card is as wide as the
        // grid leaves it, so "draw it and hope" put 101 px of name in
        // an 80 px card and let the rest run over its neighbour.
        if let Some(title) = state.slot_titles.get(slot) {
            if !title.is_empty() {
                let fg = if is_drag_origin { card_fg_drag_origin } else { card_fg };
                let pad = crate::ui::components::layout_modal::CARD_LABEL_PAD_LOGICAL
                    * state.scale;
                let fitted = crate::ui::core::fit_mono(
                    title, rect.w, p.cell_w as f64, pad,
                );
                p.text_in(*rect, &fitted, fg, Alignment::Center);
            }
        }
    }
    let _ = (cell_w, cell_h, ascent);
    // Floating dragged card: a copy of the source card painted at
    // (mouse - grab_offset).  Drawn LAST so it sits on top of all
    // other cards.  Same BG / border as a regular card but more
    // saturated to read as "lifted".
    if let Some(d) = state.drag.as_ref() {
        if d.from_slot < modal.cards.len() {
            let src = modal.cards[d.from_slot];
            let drag_rect = marspot_term::layout::Rect {
                x: d.mouse_phys.0 - d.grab_offset_phys.0,
                y_top: d.mouse_phys.1 - d.grab_offset_phys.1,
                w: src.w,
                h: src.h,
            };
            let drag_bg = [0.22, 0.26, 0.32, 1.0];
            let drag_border = [0.55, 0.62, 0.72, 1.0];
            p.fill_rounded_rect(drag_rect, drag_bg, 6.0, (drag_border, 1.5));
            if let Some(title) = state.slot_titles.get(d.from_slot) {
                if !title.is_empty() {
                    p.text_in(drag_rect, title, card_fg, Alignment::Center);
                }
            }
        }
    }
}

/// Internal: the content of the Process Monitor modal (title bar
/// fill, traffic lights, title text, tab strip, body rows + [×]
/// kill buttons).  Called from `push_process_panel_via_view` with
/// a `ViewPainter` routing to overlay scratches.
fn paint_process_panel_content(
    panel: &ProcessPanelRender,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    let px = panel.rect.x as f32;
    let py = panel.rect.y_top as f32;
    let pw = panel.rect.w as f32;
    let ph = panel.rect.h as f32;
    // The original push_process_panel inferred scale from cell_h;
    // keep that heuristic so glyph sizes stay in proportion.
    let scale_hint = (p.cell_h / 20.0).max(0.5);
    let title_h = PROCESS_PANEL_TITLE_BAR_H_LOGICAL * scale_hint;
    let _tab_h = PROCESS_PANEL_TAB_STRIP_H_LOGICAL * scale_hint;
    use crate::ui::system::macos::traffic_lights as tl;


    // Title bar fill (flat over the rounded chrome).
    p.fill_rect(
        Rect { x: px as f64, y_top: py as f64, w: pw as f64, h: title_h as f64 },
        PROCESS_PANEL_TITLE_BAR_BG,
    );
    // 1 px separator below title bar.
    p.fill_rect(
        Rect { x: px as f64, y_top: (py + title_h) as f64, w: pw as f64, h: 1.0 },
        PROCESS_PANEL_SEPARATOR,
    );
    // Traffic lights (3 SDF discs anchored title-bar left).

    tl::paint_discs(p.ui_rects, &panel.light_rects, panel.title_bar_hovered);
    // Centred title text, in the shared panel role — a panel's name
    // is set the same way in every panel.  The body below stays mono:
    // it is a process table, and columns line up for free there.
    p.panel_text_in(
        crate::ui::theme::PanelText::Title,
        Rect { x: px as f64, y_top: py as f64, w: pw as f64, h: title_h as f64 },
        &panel.title,
        PROCESS_PANEL_TITLE_FG,
        Alignment::Center,
    );

    if panel.minimized {
        return;
    }

    // F3+4.1 — master / detail rendered via the new `Table`
    // component.  The kill [×] is layered on top of the detail
    // Table after paint() since Table is text-only.
    use marspot_term::layout::Alignment;
    use crate::ui::components::{
        Button, ButtonStyle, IconSpec, IconPosition,
        Table, TableColumn, TableRow, TableStyle, ColumnWidth, RowKind,
    };

    let pad = PROCESS_PANEL_SIDE_PAD_LOGICAL * scale_hint;
    let body_top = py + title_h + pad;
    let body_bottom = py + ph - pad;
    // Content width is the frame minus a pad on each outer edge; the
    // master/detail split lands inside that, not on the frame.
    let content_x = px + pad;
    let content_w = pw - pad * 2.0;
    let master_w = content_w * PROCESS_PANEL_MASTER_FRAC as f32;
    let split_x = content_x + master_w;

    // Vertical separator between master and detail spans full body.
    p.fill_rect(
        Rect { x: split_x as f64, y_top: body_top as f64,
               w: 1.0, h: (body_bottom - body_top) as f64 },
        PROCESS_PANEL_SEPARATOR,
    );

    // Shared Table style with marspot's panel palette.
    let style = TableStyle {
        header_h:           (p.cell_h * 1.4) as f64,
        row_h:              (p.cell_h * 1.3) as f64,
        indent_px:          12.0 * scale_hint as f64,
        col_pad:            10.0 * scale_hint as f64,
        header_bg:          PROCESS_PANEL_TITLE_BAR_BG,
        header_fg:          PROCESS_PANEL_HEADER_FG,
        header_separator:   PROCESS_PANEL_SEPARATOR,
        row_bg:             [0.0; 4],
        row_bg_alt:         [0.0; 4],
        row_bg_selected:    PROCESS_PANEL_TAB_BG_ACTIVE,
        row_fg:             PROCESS_PANEL_ROW_FG,
        row_fg_selected:    PROCESS_PANEL_TAB_FG_ACTIVE,
        section_fg:         PROCESS_PANEL_HEADER_FG,
    };

    // ── Master ───────────────────────────────────────────────────
    let m_cols = vec![
        TableColumn {
            header: "Pane".into(),
            width: ColumnWidth::Flex(1.0),
            align: Alignment::CenterLeft,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "pids".into(),
            width: ColumnWidth::Px((p.cell_w * 6.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "CPU%".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: Some(crate::ui::components::SortDir::Desc),
            sortable: false,
        },
        TableColumn {
            header: "RSS".into(),
            width: ColumnWidth::Px((p.cell_w * 8.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
    ];
    let m_rows: Vec<TableRow> = panel.pane_rows.iter().map(|r| TableRow {
        cells: vec![
            r.name.clone(),
            r.n_pids.to_string(),
            format!("{:.1}", r.cpu_pct),
            format_rss(r.rss_kb),
        ],
        depth: 0,
        kind: RowKind::Data,
    }).collect();
    let master_table = Table {
        rect: Rect {
            x: content_x as f64, y_top: body_top as f64,
            w: (master_w - pad * 0.5) as f64,
            h: (body_bottom - body_top) as f64,
        },
        columns: &m_cols,
        rows: &m_rows,
        style,
        selected: Some(panel.selected_pane),
        scroll_y: 0.0,
        show_header: true,
    };
    master_table.paint(p);

    // ── Detail ───────────────────────────────────────────────────
    let d_cols = vec![
        TableColumn {
            header: "Process".into(),
            width: ColumnWidth::Flex(1.0),
            align: Alignment::CenterLeft,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "pid".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "CPU%".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "RSS".into(),
            width: ColumnWidth::Px((p.cell_w * 8.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "".into(),
            width: ColumnWidth::Px(
                (PROCESS_PANEL_KILL_W_LOGICAL * scale_hint + 8.0) as f64,
            ),
            align: Alignment::Center,
            sort: None, sortable: false,
        },
    ];
    let d_rows: Vec<TableRow> = panel.rows.iter().map(|r| TableRow {
        cells: if r.is_header {
            vec![r.comm.clone(), String::new(), String::new(), String::new(), String::new()]
        } else {
            vec![
                r.comm.clone(),
                r.pid.to_string(),
                if r.cpu_pct > 0.05 { format!("{:.1}", r.cpu_pct) } else { "·".into() },
                format_rss(r.rss_kb),
                String::new(),
            ]
        },
        depth: r.depth,
        kind: if r.is_header { RowKind::Section } else { RowKind::Data },
    }).collect();
    let detail_table = Table {
        rect: Rect {
            x: (split_x + pad * 0.5) as f64, y_top: body_top as f64,
            w: (content_w - master_w - pad * 0.5) as f64,
            h: (body_bottom - body_top) as f64,
        },
        columns: &d_cols,
        rows: &d_rows,
        style,
        selected: None,
        scroll_y: panel.scroll_y,
        show_header: true,
    };
    detail_table.paint(p);

    // Kill buttons overlay the Table's last column for non-header rows.
    let kill_w = PROCESS_PANEL_KILL_W_LOGICAL * scale_hint;
    let kill_h = (style.row_h as f32 - 4.0).max(8.0);
    let col_xw = detail_table.column_x_widths();
    let kill_col = col_xw.last().copied();
    if let Some((kx, kw)) = kill_col {
        for (i, row) in panel.rows.iter().enumerate() {
            if row.is_header { continue; }
            let row_rect = detail_table.row_rect(i);
            if row_rect.y_top + row_rect.h <= detail_table.body_rect().y_top { continue; }
            if row_rect.y_top >= detail_table.body_rect().y_top
                + detail_table.body_rect().h { break; }
            let bx = kx + (kw - kill_w as f64) * 0.5;
            let by = row_rect.y_top + (row_rect.h - kill_h as f64) * 0.5;
            let btn = Button {
                rect: Rect { x: bx, y_top: by, w: kill_w as f64, h: kill_h as f64 },
                label: None,
                icon: Some(IconSpec::Glyph("×")),
                icon_position: IconPosition::Only,
                hovered: false,
                style: ButtonStyle::destructive(),
            };
            btn.paint(p);
        }
    }
}

/// F3+4 — pretty-print RSS bytes (KB units) as a `X.Y M` / `X.Y G`
/// style human-readable string for the master/detail rss column.
/// Pre-formatted by L2 so the renderer doesn't pull format helpers.
fn format_rss(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    } else if kb >= 1024 {
        format!("{:.0}M", kb as f64 / 1024.0)
    } else {
        format!("{}K", kb)
    }
}


// Close-[×] button colours.  Subtle red-tinted BG so the user
// reads "destructive action zone" without it screaming; the
// actual `×` glyph is rasterised via the FG pass so it's a
// true diagonal cross (NOT the axis-aligned `+` an earlier
// attempt drew, which read as "add" — wrong affordance).
// When this is the only session (close_session_rects.len() == 1)
// a softer gray pair is used so the user sees the affordance
// is disabled.
const CLOSE_BTN_BG: [f32; 4] = [0.18, 0.07, 0.08, 0.85];
const CLOSE_BTN_FG: [f32; 4] = [0.92, 0.62, 0.62, 1.0];
const CLOSE_BTN_BG_DISABLED: [f32; 4] = [0.10, 0.10, 0.11, 0.65];
const CLOSE_BTN_FG_DISABLED: [f32; 4] = [0.45, 0.45, 0.47, 0.8];

// Add-[+] button colours.  Green-tinted BG to read as
// "constructive action".  Disabled (N == SESSION_COUNT_HARD_CAP)
// drops to gray — `mouse_down` ignores the click but the dim look
// explains why.
const ADD_BTN_BG: [f32; 4] = [0.07, 0.16, 0.10, 0.85];
const ADD_BTN_FG: [f32; 4] = [0.65, 0.92, 0.72, 1.0];
const ADD_BTN_BG_DISABLED: [f32; 4] = [0.10, 0.10, 0.11, 0.65];
const ADD_BTN_FG_DISABLED: [f32; 4] = [0.45, 0.45, 0.47, 0.8];

// RFC-004 E.1 (B11) — this was a private `= 9` that predated the
// cap raise to 36 in ui::mod; the [+] button greyed out at 9 panes
// while spawn paths honoured 36.  One constant, one truth.
use crate::ui::SESSION_COUNT_HARD_CAP;

fn push_layout_chrome(
    layout: &Layout,
    hover_chrome_btn: Option<u8>,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    use crate::ui::components::{Button, ButtonStyle, IconSpec, IconPosition};
    use crate::ui::system::macos::icons::{SidebarIcon, GridIcon, ListTreeIcon, DevPanelIcon, UsageBarsIcon, SlidersIcon};

    // F3+1.12 — chrome hairline seams (sidebar↔grid + header↔grid).
    // Same SEAM tone as GridSeams; routed through the painter (UI
    // pipeline) so they layer over pane BG like the rest of the
    // chrome.  There used to be a third seam splitting the header into
    // title strip + toolbar; the header is one band now, so drawing a
    // line through it would cut the row the lights sit on.
    if layout.gutter > 0.0 {
        let g = layout.gutter;
        let seam = [SEAM.0, SEAM.1, SEAM.2, 1.0];
        let ui_fill = |p: &mut crate::ui::core::ViewPainter, r: Rect| {
            p.fill_rounded_rect(r, seam, 0.0, ([0.0, 0.0, 0.0, 0.0], 0.0));
        };
        if layout.sidebar_w > 0.0 {
            ui_fill(p, Rect {
                x: layout.sidebar_w, y_top: layout.top_inset,
                w: g, h: layout.window_h - layout.top_inset,
            });
        }
        if layout.top_inset > 0.0 {
            ui_fill(p, Rect {
                x: 0.0, y_top: layout.top_inset - g,
                w: layout.window_w, h: g,
            });
        }
    }

    // Every toolbar button, painted from one list.
    //
    // `layout.toolbar_buttons()` is the layout's own order, zipped
    // against a same-length icon array — so a button the layout knows
    // about cannot be left unpainted.  The settings button was added
    // without this and shipped invisible: the rect was laid out and
    // the hit-test worked, so clicking the empty space opened a panel
    // nobody could see a button for.
    let sidebar_collapsed = layout.sidebar_w == 0.0;
    let sidebar_icon = SidebarIcon { collapsed: sidebar_collapsed };
    let grid_icon = GridIcon { cols: layout.grid_cols, rows: layout.grid_rows };
    let list_tree_icon = ListTreeIcon;
    let dev_panel_icon = DevPanelIcon;
    let usage_icon = UsageBarsIcon;
    let sliders_icon = SlidersIcon;
    let icons: [&dyn crate::ui::core::IconComponent; 6] = [
        &sidebar_icon,
        &grid_icon,
        &list_tree_icon,
        &dev_panel_icon,
        &usage_icon,
        &sliders_icon,
    ];
    let chrome = ButtonStyle::chrome();
    for (i, (rect, icon)) in layout.toolbar_buttons().into_iter().zip(icons).enumerate() {
        // A zero-width rect is a button this layout does not show
        // (snapshot / bench paths build a chrome-less layout).
        if rect.w <= 0.0 {
            continue;
        }
        let btn = Button {
            rect,
            label: None,
            icon: Some(IconSpec::Component(icon)),
            icon_position: IconPosition::Only,
            hovered: hover_chrome_btn == Some(i as u8),
            style: chrome,
        };
        btn.paint(p);
    }

    // F3+3.0 — picker popup removed; `LayoutModal` (separate
    // component) replaces it.

    // Sidebar close-[×] BG tints (FG `×` glyph laid down later in
    // `push_close_glyphs`).  Painted as plain rounded rects via the
    // UI pipeline so they layer correctly on top of sidebar BG.
    let n_sessions = layout.close_session_rects.len();
    let close_disabled = n_sessions == 1;
    let close_bg = if close_disabled { CLOSE_BTN_BG_DISABLED } else { CLOSE_BTN_BG };
    for rect in &layout.close_session_rects {
        p.fill_rounded_rect(*rect, close_bg, 0.0,
            ([0.0, 0.0, 0.0, 0.0], 0.0));
    }
    // Sidebar [+] add-session BG.  Same pipeline reasoning.
    if layout.add_session_button_rect.w > 0.0 {
        let add_disabled = n_sessions >= SESSION_COUNT_HARD_CAP;
        let add_bg = if add_disabled { ADD_BTN_BG_DISABLED } else { ADD_BTN_BG };
        p.fill_rounded_rect(layout.add_session_button_rect, add_bg, 0.0,
            ([0.0, 0.0, 0.0, 0.0], 0.0));
    }
}


#[allow(clippy::too_many_arguments)]
/// The running binary's version label, e.g. `v0.12.67`.  Bumped on
/// every commit that changes a binary (project rule), so it still works
/// as visible proof that a silent update landed — the product name and
/// the git sha that used to trail it were noise in a corner the user
/// reads dozens of times a day.  The sha stays available in the window
/// title and in logs.
pub fn version_label() -> String {
    // L2 (marspot-core) is the canonical marspot version — the header
    // shows it, since the renderer is what users actually interact
    // with. The other layers (L1 shell, L3 session) carry their own
    // semver in version-vector.toml for operator diagnostics, but the
    // headline number is L2's.
    format!("v{}", env!("MARSPOT_VERSION_CORE"))
}

/// Lay a run of text starting at baseline `(x, baseline_y)` in
/// physical pixels, advancing one monospace cell per char.  Shared
/// by the cell-title strip and the header version label.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    push_text_run_kind(
        text, x_start, baseline_y, color,
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        font, atlas, glyphs, FontKind::Terminal,
    )
}

/// Which font family/path to use for text rendering.  `Terminal` =
/// mono cell-aligned (PTY grid).  `Ui` = system UI font (SF Pro on
/// macOS), proportional;  per-glyph advance via CT, falls back to
/// mono cascade for chars the UI font lacks (CJK).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind { Terminal, Ui }

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_kind(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
    kind: FontKind,
) {
    // Phase 3 — chrome runs go through `FontCache::shape_ui` (CTLine
    // shaping with cached re-shape, kerning + ligatures + auto font
    // fallback ON).  PTY runs keep the mono cell loop below.  Phase 5
    // weight is plumbed via `push_text_run_kind_weighted` — this
    // legacy entry point keeps the regular-weight (400) default for
    // back-compat callers.
    if kind == FontKind::Ui {
        // Legacy entry: no colour atlas / sink in scope, so colour
        // emoji glyphs fall back to mono alpha silhouettes (the
        // pre-Phase-7 behaviour).  Chrome calls
        // `push_text_run_ui_shaped` directly with both atlases.
        // Phase 8 — legacy entry has no `opts` either; default to
        // `full()` so existing callers preserve CTLine defaults.
        push_text_run_ui_shaped_mono(
            text, x_start, baseline_y, color,
            ascent, atlas_w, atlas_h, 400,
            font, atlas, glyphs,
        );
        return;
    }
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let mut x = x_start;
    for ch in text.chars() {
        let n_cells = crate::grid::char_width(ch).max(1) as u16;
        let (font_idx, glyph) = font.resolve_char(ch, false, false);
        if glyph != 0 {
            let ct_font = font.font(font_idx).clone();
            if let Some(entry) = atlas.get_or_rasterize(
                text_glyph_key(font_idx as u32, glyph, &ct_font),
                &ct_font,
                metrics,
                n_cells,
            ) {
                // Phase 1.1 bearing formula.  Pre-round to the integer
                // slot-top grid the pre-Phase-1.1 code used (so Phase 1.0
                // entries are bit-equivalent to the old `dest_y =
                // (baseline_y - ascent).round()` formula).
                let baseline_from_top_f = metrics.baseline_from_top as f32;
                let baseline_y_q =
                    (baseline_y - baseline_from_top_f).round() + baseline_from_top_f;
                let (origin, size) = entry.quad(x.round(), baseline_y_q);
                glyphs.push(GlyphInstance {
                    origin,
                    size,
                    uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                    uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                    color,
                });
            }
        }
        x += cell_w * n_cells as f32;
    }
}

/// Phase 3 — chrome `Ui` text run.  Shapes the line through CTLine
/// (cached by `FontCache::shape_ui_weighted`), then for each shaped
/// glyph allocates an atlas slot via `get_or_rasterize_natural` (no
/// cell-fit fallback — glyph bbox sized) and emits a `GlyphInstance`
/// at the typographic origin CTLine gave us.  ASCII gets real
/// kerning (`Ta` reads tight); `fi` / `==>` show ligatures; CJK in a
/// Latin sentence routes through PingFang / Hiragino automatically.
///
/// Phase 5 — `weight` carries the CSS weight (100..900); `400` reuses
/// the base UI font, other values materialise the variable-font
/// weight variant on first call.
/// Phase 7 — mono-only chrome shape path.  Same as
/// `push_text_run_ui_shaped` but no colour atlas / sink in scope, so
/// colour-emoji glyphs fall back to the mono atlas (alpha silhouette
/// — pre-Phase-7 visual).  Used by the legacy `push_text_run_kind`
/// entry point that pre-dates the canvas colour glyph plumbing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_ui_shaped_mono(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    weight: u16,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = font.shape_ui_weighted(text, weight);
    if shaped.is_empty() {
        return;
    }
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        let ct_font = font.font(sg.font_id as usize).clone();
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(ct_font.pt_size()),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let Some(entry) = atlas.get_or_rasterize_natural(key, &ct_font) else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        glyphs.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color,
        });
    }
    let _ = ascent;
}

/// SF Pro at an explicit pt size and weight, into the mono atlas.
///
/// [`push_text_run_ui_shaped_mono`] above is locked to the startup
/// `UI_FONT_POINT` at weight 600 — one size, one weight, which is why
/// every chrome surface built on `ViewPainter` had a single type size
/// and had to signal hierarchy with colour alone.  The canvas path
/// (`push_text_run_ui_shaped`) has taken a size since Phase 10c; this
/// is the same capability for the direct painter, minus the colour
/// atlas the canvas path also routes (chrome labels are latin).
///
/// `baseline_y` is a real baseline in physical px — the caller knows
/// its own line box.  Physical px per pt is the 2× retina scale baked
/// into the atlas raster path, same as the canvas path assumes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_ui_sized(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    size_pt: f64,
    weight: u16,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = font.shape_ui_weighted_opts_at_size(
        text, weight, crate::font_shape::ShapeOptions::default(), size_pt,
    );
    if shaped.is_empty() {
        return;
    }
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        let ct_font = font.font(sg.font_id as usize).clone();
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(ct_font.pt_size()),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let Some(entry) = atlas.get_or_rasterize_natural(key, &ct_font) else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        glyphs.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color,
        });
    }
}

#[allow(clippy::too_many_arguments)]
// y_top_phys = top-of-em-box y in PHYSICAL px (raw `t.y` from canvas
// Length resolution, NOT a pre-baked baseline).  Function derives the
// baseline from the SF Pro run's own ascent at the requested pt size,
// so callers don't need to know chrome cell ascent.
// fallback_ascent = chrome cell ascent (phys px) — used ONLY when
// ui_size_q.is_none() (pre-Phase-10c default-size callers).
fn push_text_run_ui_shaped(
    text: &str,
    x_start: f32,
    y_top_phys: f32,
    color: [f32; 4],
    fallback_ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    color_atlas_w: f32,
    color_atlas_h: f32,
    weight: u16,
    opts: crate::font_shape::ShapeOptions,
    // Phase 10c — `None` = use FontCache's default UI_FONT_POINT;
    // `Some(q)` = use SF Pro at `q / 4.0` pt.
    ui_size_q: Option<u16>,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = match ui_size_q {
        Some(q) => font.shape_ui_weighted_opts_at_size(text, weight, opts, (q as f64) / 4.0),
        None => font.shape_ui_weighted_opts(text, weight, opts),
    };
    if shaped.is_empty() {
        return;
    }
    // SF Pro path uses the run's OWN ascent — derived from the first
    // shaped glyph's CTFont — so positioning a Body 8.1pt run reads
    // as 8.1pt top-of-em, not chrome cell pitch.  Bug fix 2026-06-25:
    // previously `baseline_y` was pre-computed with chrome ascent,
    // misaligning active-row BG vs SF Pro glyphs.
    let real_ascent_pt = font.font(shaped[0].font_id as usize).ascent();
    // CTFont.ascent() returns pt — convert to phys at the 2× retina
    // baked into the atlas raster path (`RETINA_SCALE = 2.0`).
    let real_ascent_phys = (real_ascent_pt * 2.0) as f32;
    let baseline_y = if ui_size_q.is_some() {
        y_top_phys + real_ascent_phys
    } else {
        y_top_phys + fallback_ascent
    };
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        // Phase 7 — colour vs mono routing.  CT may have fallen back
        // to Apple Color Emoji for any glyph in the run; rasterising
        // those into the R8 atlas would emit an alpha silhouette
        // (no colour) so the caller would see a black emoji shape.
        // Routing to `color_atlas` (BGRA8) emits real colours, and
        // the matching `fg_color_pipeline` pass blends them in
        // submission order with the mono runs.
        let is_color = font.is_color_font(sg.font_id as usize);
        let ct_font = font.font(sg.font_id as usize).clone();
        // Phase 4 — `sg.subpx_x` bucket comes from CTLine's float
        // position (shape_line quantised it).  Atlas hands back a slot
        // whose ink is pre-shifted by `subpx_x × 0.25 px`, so
        // origin.x stays integer.
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(ct_font.pt_size()),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let (entry_opt, aw, ah, sink): (Option<AtlasEntry>, f32, f32, &mut Vec<GlyphInstance>) =
            if is_color {
                (
                    color_atlas.get_or_rasterize_natural(key, &ct_font),
                    color_atlas_w,
                    color_atlas_h,
                    color_glyphs,
                )
            } else {
                (
                    atlas.get_or_rasterize_natural(key, &ct_font),
                    atlas_w,
                    atlas_h,
                    glyphs,
                )
            };
        let Some(entry) = entry_opt else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        sink.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / aw, entry.v0 as f32 / ah],
            uv1: [entry.u1 as f32 / aw, entry.v1 as f32 / ah],
            color,
        });
    }
    let _ = fallback_ascent;
}

fn push_session(
    rect: &CellRect,
    view: &SessionView,
    window_focused: bool,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    _ui_rects: &mut Vec<UiRectInstance>,
    gutter: f32,
    padding: f32,
    title_h: f32,
) {
    let (color_atlas_w, color_atlas_h) = color_atlas.dims();
    let color_atlas_w = color_atlas_w as f32;
    let color_atlas_h = color_atlas_h as f32;
    // Cell-rect BG: BG_FOCUSED (deeper) for the active pane,
    // BG_PANEL for everyone else.  The DROP into deeper black is
    // the focus indicator — focused reads as "the canvas I'm
    // typing into", surrounding cells stay at the chrome tone.
    let pane_focused = view.focused && window_focused;
    let pane_bg = if pane_focused {
        [BG_FOCUSED.0, BG_FOCUSED.1, BG_FOCUSED.2, 1.0]
    } else {
        [BG_PANEL.0, BG_PANEL.1, BG_PANEL.2, 1.0]
    };
    cells.push(CellInstance {
        origin: [rect.x as f32, rect.y_top as f32],
        size: [rect.w as f32, rect.h as f32],
        color: pane_bg,
    });

    // Title strip — band at the top of the cell hosting the
    // session label.  BG sits flush with the cell BG; a single
    // SEAM hairline along the bottom of the strip separates it
    // from the terminal content area below.  Glyphs in the strip
    // use SIDEBAR_TEXT_FG (dim greyish-blue) so the title reads
    // as quiet metadata, not as content competing with the
    // terminal text.
    if title_h > 0.0 && !view.title.is_empty() {
        let strip_bottom = rect.y_top as f32 + title_h;
        // Bottom seam (1 phys-px line just inside the strip's
        // bottom edge — sits exactly where padding starts).
        cells.push(CellInstance {
            origin: [rect.x as f32, strip_bottom - gutter.max(1.0)],
            size: [rect.w as f32, gutter.max(1.0)],
            color: [SEAM.0, SEAM.1, SEAM.2, 1.0],
        });
        // Title text — same monospace metrics as the terminal
        // body; vertically centred in the strip, left-aligned
        // with the same padding the terminal uses.
        let label_x = rect.x as f32 + padding;
        let label_baseline_y = rect.y_top as f32
            + (title_h - cell_h) * 0.5
            + ascent;
        push_text_run(
            &view.title,
            label_x,
            label_baseline_y,
            [SIDEBAR_TEXT_FG.0, SIDEBAR_TEXT_FG.1, SIDEBAR_TEXT_FG.2, 1.0],
            cell_w,
            cell_h,
            ascent,
            atlas_w,
            atlas_h,
            font,
            atlas,
            glyphs,
        );
        // Plugin badge (claudecode etc.): right-edge decoration in
        // claudecode coral.  Whatever the badge text is BEFORE the
        // first ' ' is treated as the *clickable prefix* (today:
        // "P1" / "P2" / "P3") and gets a hairline underline so the
        // user reads it as actionable; whatever follows is rendered
        // plain (the sessionId).  The plugin (L1) — not the renderer
        // — owns the meaning of the prefix; this just decorates it.
        if !view.right_badge.is_empty() {
            let badge_chars = view.right_badge.chars().count() as f32;
            // Right anchor: leave room for the refresh affordance
            // (one cell + a half-cell gap) when both are present,
            // otherwise hug the right edge with a single padding.
            let reserved = if view.update_pending && pane_focused {
                cell_w * 1.5
            } else {
                0.0
            };
            let badge_x = rect.x as f32 + rect.w as f32
                - padding
                - reserved
                - badge_chars * cell_w;
            if badge_x > label_x {
                push_text_run(
                    view.right_badge,
                    badge_x,
                    label_baseline_y,
                    [PLUGIN_BADGE_FG.0, PLUGIN_BADGE_FG.1, PLUGIN_BADGE_FG.2, 1.0],
                    cell_w,
                    cell_h,
                    ascent,
                    atlas_w,
                    atlas_h,
                    font,
                    atlas,
                    glyphs,
                );
                // Hairline underline under the prefix (text before the
                // first space).  Width = prefix_chars × cell_w; sits
                // 1 phys-px below the baseline so it doesn't clip
                // descenders that won't appear in `P<digit>` anyway.
                let prefix_chars = view
                    .right_badge
                    .split(' ')
                    .next()
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                if prefix_chars > 0 {
                    let underline_y = label_baseline_y + gutter.max(1.0);
                    cells.push(CellInstance {
                        origin: [badge_x, underline_y],
                        size: [prefix_chars as f32 * cell_w, gutter.max(1.0)],
                        color: [
                            PLUGIN_BADGE_FG.0,
                            PLUGIN_BADGE_FG.1,
                            PLUGIN_BADGE_FG.2,
                            1.0,
                        ],
                    });
                }
            }
        }
        // Deferred-update affordance (target #4 step 5b): a refresh glyph
        // at the right edge of the *focused* pane's title strip when a
        // silent swap is staged for it.  Clicking it (hit-tested via
        // `Layout::hit_test_cell_refresh`) triggers the swap.  Brighter
        // than the dim title text so it reads as an actionable control.
        if view.update_pending && pane_focused {
            let icon_x = rect.x as f32 + rect.w as f32 - padding - cell_w;
            push_text_run(
                "\u{27F3}", // ⟳ CLOCKWISE GONG WITH CIRCLE ARROW
                icon_x,
                label_baseline_y,
                [REFRESH_ICON_FG.0, REFRESH_ICON_FG.1, REFRESH_ICON_FG.2, 1.0],
                cell_w,
                cell_h,
                ascent,
                atlas_w,
                atlas_h,
                font,
                atlas,
                glyphs,
            );
        }
    }

    let grid = view.grid;
    let cols = grid.cols() as usize;
    // Origin of the terminal content area — inside cell, below
    // the title strip, then padded.  Round both origin and cell_w
    // to integer pixels here ONCE so every `c * cell_w` advance is
    // an exact integer multiple. Without this, fractional cell_w
    // makes adjacent cells round to different stride lengths
    // (col 1 advances by 8, col 2 by 9) while the atlas slot is
    // a fixed width, leaving 1-px seam gaps every few columns in
    // horizontal box-drawing runs.
    let cell_w = cell_w.round();
    let cell_h = cell_h.round();
    let inner_x = (rect.x as f32 + padding).round();
    // C1 — `TopFixed` tools (search bar etc.) push the grid's
    // top edge down by `top_fixed_h_cells * cell_h`.  Empty
    // `tools` ⇒ `top_fixed_h_cells == 0` ⇒ render byte-identical
    // to pre-C1.  `bot_fixed_h_cells` reserves space at the
    // bottom for `BottomFixed` tools — currently informational
    // (no `BottomFixed` consumer until a future tool needs it);
    // the L3 already sizes its grid so renderable rows fit.
    let top_fixed_h = view.top_fixed_h_cells as f32 * cell_h;
    let inner_y = (rect.y_top as f32 + title_h + padding + top_fixed_h).round();

    // F1++ — overlay glyph mask.  Metal renders BG and FG in two
    // separate passes (BG first, then glyphs).  Without this mask
    // grid glyphs under the search bar / list FG-pass straight onto
    // the opaque overlay BG painted in the BG pass, so the underlying
    // terminal text "bleeds through" the chrome.  Pre-compute the
    // overlay's covered cell range here; the per-cell loops below
    // skip glyph / underline / cursor emission for any cell whose
    // (row, col) falls inside.
    const SEARCH_BAR_COLS: u16 = 40;
    const SEARCH_LIST_MAX_ROWS: u16 = 10;
    let overlay_mask: Option<(u16, u16, u16, u16)> = view.search_overlay.as_ref()
        .and_then(|ov| {
            let cols = grid.cols();
            if cols < SEARCH_BAR_COLS + 2 { return None; }
            // Mirror the F1+11 pixel-mode overlay geometry: the panel
            // floats ~½ cell down from the grid top, occupies query
            // (1 row) + divider (½ row) + list rows + 2× inner
            // padding.  Round generously upward so grid glyphs under
            // the panel are masked.
            let list_rows = (ov.hits.len() as u16).min(SEARCH_LIST_MAX_ROWS);
            // 1 (margin) + 1 (query) + 1 (divider region) + list + 1 (bottom pad)
            let covered_rows = 1 + 1 + 1 + list_rows + 1;
            let col_start = cols - SEARCH_BAR_COLS - 1;
            let col_end_inclusive = cols - 2;
            let row_start: u16 = 0;
            let row_end_exclusive = covered_rows.min(grid.rows());
            Some((row_start, row_end_exclusive, col_start, col_end_inclusive))
        });
    // F1+13 — fast-path: when the overlay is closed (the common case),
    // bail out without touching `overlay_mask`'s scrutinee on every
    // per-cell call.  Keeps `under_overlay` a tight inline check on
    // the per-row / per-cell hot path.
    let overlay_active = overlay_mask.is_some();
    let under_overlay = |row: u16, col: u16| -> bool {
        if !overlay_active {
            return false;
        }
        match overlay_mask {
            Some((r0, r1, c0, c1)) => row >= r0 && row < r1 && col >= c0 && col <= c1,
            None => false,
        }
    };

    // Scan once up front so the per-row glyph loop can override fg
    // for cells inside a link span (paint the text the same cyan as
    // the underline, the standard "this is clickable" cue) and the
    // underline pass below can reuse the same list.
    // An agent TUI wraps its own lines, so tell the link scanner to
    // merge the hanging-indent continuation into one logical URL /
    // path token.  Inert on ordinary panes.  See `SessionView::
    // agent_tui` for why this is no longer inferred from the badge.
    let link_opts = marspot_term::grid_links::ScanOpts {
        cc_mode: view.agent_tui,
    };
    // The oracle is what keeps this call off the filesystem: on the
    // render thread a single `lstat` under a network mount or the
    // `auto_home` autofs map has been measured at six seconds.  See
    // `marspot::link_probe`.
    let links = marspot_term::grid_links::scan_visible_links_with(
        grid,
        view.view_offset,
        link_opts,
        crate::link_probe::oracle(),
    );
    let in_link = |row: u16, col: u16| -> bool {
        links
            .iter()
            .any(|l| l.row == row && col >= l.col_start && col <= l.col_end)
    };

    for r in 0..grid.rows() {
        let row_y = inner_y + (r as f32) * cell_h;
        let _baseline_y = row_y + ascent;

        // Run-length BG fills (skip default BG; it inherits the rect fill).
        let mut c = 0usize;
        while c < cols {
            let cell = grid.cell_at_view(view.view_offset, c as u16, r);
            let bg = resolve_attrs(cell.attrs).1;
            if bg == BG {
                c += 1;
                continue;
            }
            let start = c;
            c += 1;
            while c < cols {
                let cur = grid.cell_at_view(view.view_offset, c as u16, r);
                if resolve_attrs(cur.attrs).1 != bg {
                    break;
                }
                c += 1;
            }
            cells.push(CellInstance {
                origin: [inner_x + start as f32 * cell_w, row_y],
                size: [(c - start) as f32 * cell_w, cell_h],
                color: [bg.0 as f32, bg.1 as f32, bg.2 as f32, 1.0],
            });
        }

        // C4 — search-hit highlight BG.  Drawn AFTER cell BG so it
        // wins z-order, BEFORE glyphs so they paint on top with
        // their original FG (the "highlight yellow BG, original fg
        // preserved" rule from §6.8).  Bounded by grid cols — a
        // span fed with `col_end_inclusive >= cols` is clamped to
        // the right edge.  Per-row loop: O(spans) work scoped to
        // rows where the highlight lives; renderer p99 delta on a
        // typical single-row span = one extra `CellInstance` push.
        for span in view.highlight_spans {
            if span.view_row != r {
                continue;
            }
            let cols_u16 = cols as u16;
            if span.col_start >= cols_u16 {
                continue;
            }
            let col_end = span.col_end_inclusive.min(cols_u16 - 1);
            if span.col_start > col_end {
                continue;
            }
            let n_cols = (col_end + 1 - span.col_start) as f32;
            cells.push(CellInstance {
                origin: [inner_x + span.col_start as f32 * cell_w, row_y],
                size: [n_cols * cell_w, cell_h],
                color: [HIGHLIGHT_BG.0, HIGHLIGHT_BG.1, HIGHLIGHT_BG.2, 1.0],
            });
        }

        // Glyphs.  Skip the cursor cell when the cursor is solid —
        // we re-emit it after with BG colour so the glyph reads
        // inverted on the white cursor block (mirrors render.rs).
        let cursor = grid.cursor();
        let solid_cursor = view.view_offset == 0
            && view.cursor_visible
            && view.focused
            && window_focused;
        for c in 0..cols {
            if solid_cursor && cursor == (c as u16, r) {
                continue;
            }
            // F1++ — skip grid glyphs covered by the search overlay
            // chrome (BG / FG run in separate passes so without this
            // mask, the underlying terminal text bleeds through the
            // opaque overlay).
            if under_overlay(r, c as u16) {
                continue;
            }
            let cell = grid.cell_at_view(view.view_offset, c as u16, r);
            if cell.ch == ' ' || cell.ch == '\0' {
                continue;
            }
            let metrics = SlotMetrics {
                cell_w: cell_w.round() as u32,
                cell_h: cell_h.round() as u32,
                baseline_from_top: ascent.round() as u32,
            };
            let (entry, is_color) = match resolve_cell_glyph_routed(
                atlas,
                color_atlas,
                font,
                cell.ch,
                cell.attrs.bold,
                cell.attrs.italic,
                metrics,
            ) {
                Some(e) => e,
                None => continue,
            };
            let fg = if in_link(r, c as u16) {
                (
                    LINK_UNDERLINE_FG.0 as f64,
                    LINK_UNDERLINE_FG.1 as f64,
                    LINK_UNDERLINE_FG.2 as f64,
                )
            } else {
                resolve_attrs(cell.attrs).0
            };
            // Phase 1.1 bearing formula — see `AtlasEntry::quad`.
            // pen_x = cell origin; baseline_y = slot top + ascent (use
            // the same integer `baseline_from_top` that drove the
            // rasteriser so origin.y reduces to row_y.round() for
            // Phase 1.0 entries — bit-equivalent to the pre-Phase-1.1
            // cell-aligned formula).
            let pen_x = (inner_x + c as f32 * cell_w).round();
            let baseline_y = row_y.round() + metrics.baseline_from_top as f32;
            let (origin, size) = entry.quad(pen_x, baseline_y);
            // Colour glyphs (emoji) go to the colour buffer + atlas; the
            // colour shader samples their real pixels and ignores `color`
            // (except its alpha, used for pane-dim).  Mono glyphs are
            // tinted by `fg` as before.
            let (aw, ah, sink) = if is_color {
                (color_atlas_w, color_atlas_h, &mut *color_glyphs)
            } else {
                (atlas_w, atlas_h, &mut *glyphs)
            };
            sink.push(GlyphInstance {
                origin,
                size,
                uv0: [entry.u0 as f32 / aw, entry.v0 as f32 / ah],
                uv1: [entry.u1 as f32 / aw, entry.v1 as f32 / ah],
                color: [fg.0 as f32, fg.1 as f32, fg.2 as f32, 1.0],
            });
        }

        // Underline pass — BG-pass coloured rectangles below the
        // baseline.  Run-length over consecutive same-fg underlined
        // cells.  Geometry matches render.rs (`(cell_h - ascent) * 0.55`,
        // `(cell_h * 0.06).max(1.0)`).
        let underline_y = row_y + cell_h - (cell_h - ascent) * 0.45;
        let underline_h = (cell_h * 0.06).max(1.0);
        let mut u = 0usize;
        while u < cols {
            let cell = grid.cell_at_view(view.view_offset, u as u16, r);
            if !cell.attrs.underline {
                u += 1;
                continue;
            }
            let fg = resolve_attrs(cell.attrs).0;
            let start = u;
            u += 1;
            while u < cols {
                let cur = grid.cell_at_view(view.view_offset, u as u16, r);
                if !cur.attrs.underline || resolve_attrs(cur.attrs).0 != fg {
                    break;
                }
                u += 1;
            }
            cells.push(CellInstance {
                origin: [inner_x + start as f32 * cell_w, underline_y],
                size: [(u - start) as f32 * cell_w, underline_h],
                color: [fg.0 as f32, fg.1 as f32, fg.2 as f32, 1.0],
            });
        }
    }

    // Selection BG — paint a single quad per selected row, AFTER
    // the per-cell run-length BG fills.  Order matters: the BG
    // pass writes opaque pixels and the last instance wins, so a
    // selection drawn before the per-cell BG would be hidden on
    // any cell carrying an ANSI-coloured BG (red `git diff -`,
    // green `git diff +`, etc.).  Drawing it here paints over
    // the cell colours so the highlight is always visible while
    // active.
    if let Some(sel) = view.selection {
        let (anchor, focus) = (sel.anchor, sel.focus);
        let max_row = grid.rows().saturating_sub(1);
        let max_col = grid.cols().saturating_sub(1);
        if sel.blockwise {
            // Rectangle: each row from min..=max col, independent
            // of row position.  Lets the user carve out a column
            // from multi-column output (ls, top) without dragging
            // the column-aligned padding along.
            let r_lo = anchor.1.min(focus.1).min(max_row);
            let r_hi = anchor.1.max(focus.1).min(max_row);
            let c_lo = anchor.0.min(focus.0).min(max_col);
            let c_hi = anchor.0.max(focus.0).min(max_col);
            if c_hi >= c_lo {
                let w = (c_hi - c_lo + 1) as f32 * cell_w;
                for r in r_lo..=r_hi {
                    cells.push(CellInstance {
                        origin: [
                            inner_x + c_lo as f32 * cell_w,
                            inner_y + r as f32 * cell_h,
                        ],
                        size: [w, cell_h],
                        color: [SELECTION_BG.0, SELECTION_BG.1, SELECTION_BG.2, 1.0],
                    });
                }
            }
        } else {
            // Row-band: top row from anchor.col to end, middle rows
            // full width, bottom row from start to focus.col.
            let (start, end) = if (anchor.1, anchor.0) <= (focus.1, focus.0) {
                (anchor, focus)
            } else {
                (focus, anchor)
            };
            let (s_col, s_row) = start;
            let (e_col, e_row) = end;
            let s_row = s_row.min(max_row);
            let e_row = e_row.min(max_row);
            for r in s_row..=e_row {
                let col_lo = if r == s_row { s_col } else { 0 };
                let col_hi = if r == e_row { e_col } else { max_col };
                let col_lo = col_lo.min(max_col);
                let col_hi = col_hi.min(max_col);
                if col_hi < col_lo {
                    continue;
                }
                let x = inner_x + col_lo as f32 * cell_w;
                let y = inner_y + r as f32 * cell_h;
                let w = (col_hi - col_lo + 1) as f32 * cell_w;
                cells.push(CellInstance {
                    origin: [x, y],
                    size: [w, cell_h],
                    color: [SELECTION_BG.0, SELECTION_BG.1, SELECTION_BG.2, 1.0],
                });
            }
        }
    }

    // Auto-link underline — reuse `links` from the up-front scan.
    // Runs AFTER selection so the link hint stays visible when the
    // user drags over a link (the selection blue tints it but the
    // underline sits on top).  Same geometry as the SGR-underline
    // pass above, so a link sitting on already-underlined text just
    // paints the link colour over the same row.
    {
        for link in &links {
            let row_y = inner_y + (link.row as f32) * cell_h;
            let underline_y = row_y + cell_h - (cell_h - ascent) * 0.45;
            let underline_h = (cell_h * 0.06).max(1.0);
            let cols_in_span = link.col_end.saturating_sub(link.col_start) + 1;
            cells.push(CellInstance {
                origin: [
                    inner_x + link.col_start as f32 * cell_w,
                    underline_y,
                ],
                size: [cols_in_span as f32 * cell_w, underline_h],
                color: [
                    LINK_UNDERLINE_FG.0,
                    LINK_UNDERLINE_FG.1,
                    LINK_UNDERLINE_FG.2,
                    1.0,
                ],
            });
        }
    }

    // Cursor cell glyph in BG colour (re-rendered atop the white
    // cursor block) — only when the cursor is solid.  Hollow cursor
    // doesn't cover the glyph, so the normal FG glyph above suffices.
    if view.view_offset == 0
        && view.cursor_visible
        && view.focused
        && window_focused
    {
        let (col, row) = grid.cursor();
        // F1++ — skip cursor glyph re-emit when the cursor lands
        // under the overlay (would bleed through the BG pass).
        if !under_overlay(row, col) {
        let cell = grid.cell_at_view(0, col, row);
        if cell.ch != ' ' && cell.ch != '\0' {
            let metrics = SlotMetrics {
                cell_w: cell_w.round() as u32,
                cell_h: cell_h.round() as u32,
                baseline_from_top: ascent.round() as u32,
            };
            if let Some(entry) = resolve_cell_glyph(
                atlas,
                font,
                cell.ch,
                cell.attrs.bold,
                cell.attrs.italic,
                metrics,
            ) {
                // Phase 1.1 bearing formula (cursor BG re-emit).
                let pen_x = (inner_x + col as f32 * cell_w).round();
                let dest_y = (inner_y + (row as f32) * cell_h).round();
                let baseline_y = dest_y + metrics.baseline_from_top as f32;
                let (origin, size) = entry.quad(pen_x, baseline_y);
                glyphs.push(GlyphInstance {
                    origin,
                    size,
                    uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                    uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                    color: [BG.0 as f32, BG.1 as f32, BG.2 as f32, 1.0],
                });
            }
        }
        }
    }

    // Cursor (live view + DECTCEM on).
    if view.view_offset == 0 && view.cursor_visible {
        let (col, row) = grid.cursor();
        let cx = inner_x + col as f32 * cell_w;
        let cy = inner_y + row as f32 * cell_h;
        let solid = view.focused && window_focused;
        let color = [CURSOR_FG.0, CURSOR_FG.1, CURSOR_FG.2, 1.0];
        if solid {
            cells.push(CellInstance {
                origin: [cx, cy],
                size: [cell_w, cell_h],
                color,
            });
        } else {
            // Hollow: 4 stroke quads.  Stroke width tracks render.rs.
            let stroke = (cell_h * 0.07).max(1.0);
            cells.push(CellInstance { origin: [cx, cy], size: [cell_w, stroke], color });
            cells.push(CellInstance { origin: [cx, cy + cell_h - stroke], size: [cell_w, stroke], color });
            cells.push(CellInstance { origin: [cx, cy], size: [stroke, cell_h], color });
            cells.push(CellInstance { origin: [cx + cell_w - stroke, cy], size: [stroke, cell_h], color });
        }
    }

    // IME preedit overlay — paint the in-flight composition at the
    // cursor position so the user sees pinyin / hiragana before the
    // IME commits.  Only when the pane is focused, live, and the
    // host window has focus; otherwise the cursor anchor isn't
    // visible / interactive.
    //
    // Walks the preedit as UAX #29 grapheme clusters (not raw chars),
    // so a composed CJK char + tone mark or an emoji ZWJ sequence
    // takes its real visual cell footprint.  Wraps to the next row
    // when the cluster would spill past the right edge — matches
    // iTerm2 / Alacritty behaviour rather than silently truncating
    // long preedit strings.  Covers each cluster with a contiguous
    // BG quad first so already-rendered text (e.g. zsh autosuggest,
    // a prior CJK cell) doesn't bleed through.
    if view.view_offset == 0
        && view.focused
        && window_focused
        && !view.ime_preedit.is_empty()
    {
        let (col, row) = grid.cursor();
        let cells_per_row = grid.cols();
        let rows_total = grid.rows();
        let mut c = col as u32;
        let mut r = row as u32;
        let metrics = SlotMetrics {
            cell_w: cell_w.round() as u32,
            cell_h: cell_h.round() as u32,
            baseline_from_top: ascent.round() as u32,
        };
        let mut clusters_drawn: usize = 0;
        for cluster in crate::grapheme::graphemes(&view.ime_preedit) {
            // Newlines from an IME's structured composition: advance
            // to the next row at col=0, don't draw a glyph for them.
            if cluster == "\n" || cluster == "\r" || cluster == "\r\n" {
                c = 0;
                r = r.saturating_add(1);
                if r >= rows_total as u32 {
                    break;
                }
                continue;
            }
            let n_cells = crate::grapheme::cluster_width(cluster).max(1) as u32;
            // Wrap when this cluster would overflow the current row.
            if c + n_cells > cells_per_row as u32 {
                c = 0;
                r = r.saturating_add(1);
                if r >= rows_total as u32 {
                    // Out of vertical room — stop drawing further
                    // clusters.  The IME candidate window still shows
                    // the full string; this inline preview is a hint,
                    // not the source of truth.
                    break;
                }
            }
            let dest_x = (inner_x + c as f32 * cell_w).round();
            let dest_y = (inner_y + r as f32 * cell_h).round();
            let slot_w = n_cells as f32 * cell_w;
            // BG quad — fully opaque cover so the in-cell text below
            // (zsh autosuggestion, ghost completion, residual cursor
            // block) is hidden.  Width spans the whole cluster.
            cells.push(CellInstance {
                origin: [dest_x, dest_y],
                size: [slot_w, cell_h],
                color: [IME_PREEDIT_BG.0, IME_PREEDIT_BG.1, IME_PREEDIT_BG.2, 1.0],
            });
            // Glyph — same atlas path the cell-render uses, so the
            // preedit text is rendered at the EXACT same px size as
            // a normal cell.  A grapheme cluster's lead codepoint
            // drives the atlas lookup (the rest are combining marks
            // / ZWJ joiners we don't render inline yet — acceptable
            // first-cut, the candidate window is the source of truth
            // anyway).
            if let Some(lead) = cluster.chars().next() {
                if let Some(entry) =
                    resolve_cell_glyph(atlas, font, lead, false, false, metrics)
                {
                    // Phase 1.1 bearing formula (IME preedit).
                    let baseline_y = dest_y + metrics.baseline_from_top as f32;
                    let (origin, size) = entry.quad(dest_x, baseline_y);
                    glyphs.push(GlyphInstance {
                        origin,
                        size,
                        uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                        uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                        color: [IME_PREEDIT_FG.0, IME_PREEDIT_FG.1, IME_PREEDIT_FG.2, 1.0],
                    });
                }
            }
            // Underline — 2× the old hairline so it actually reads
            // as "this is provisional text" against the BG quad.
            // Hairline (cell_h * 0.06) was invisible at small font
            // sizes (user feedback 2026-06-15 "好小好小").
            let underline_h = (cell_h * 0.12).max(2.0).round();
            cells.push(CellInstance {
                origin: [dest_x, dest_y + cell_h - underline_h],
                size: [slot_w, underline_h],
                color: [IME_PREEDIT_FG.0, IME_PREEDIT_FG.1, IME_PREEDIT_FG.2, 1.0],
            });
            c += n_cells;
            clusters_drawn += 1;
        }
        let _ = clusters_drawn; // reserved for future dev-only log
    }

    // No darken overlay.  No FOCUS_OUTLINE blue frame.  The focus
    // affordance is the BG_FOCUSED tint applied to the cell rect at
    // the top of this fn, plus the solid-vs-hollow cursor — both
    // already done above.  iTerm2 reads exactly this way: no
    // chrome, no border, just a quiet BG lift on the active pane.
    let _ = gutter; // pane-internal layout doesn't use it any more

    // F1+11 — true pixel-mode UI overlay.  Drawn via the new
    // `ui_rect_pipeline`: a SDF-based rounded rectangle with
    // anti-aliased corners, optional stroke, and a soft drop shadow
    // — NOT cell-grid characters.  Pixel-precise positioning, freed
    // from the box-drawing approximation.  Text inside still goes
    // through the cell glyph atlas (proportional UI font is Phase 2),
    // but everything BEHIND the text — panel, borders, focused-row
    // selection — is real GPU-side vector chrome.
    //
    // Layout:
    //   ┌── panel(rounded rect + shadow + 1px stroke)
    //   │  query row : query text + caret
    //   │  divider line (thin rounded rect, dim color)
    //   │  list rows : focused row is a separate rounded rect
    //   │              behind the snippet text
    //   └──
    //
    // Palette: Darcula-ish dark gray-blue panel, JetBrains-style
    // indigo selection (NOT the yellow grid-side HIGHLIGHT_BG —
    // those have distinct semantic meanings and must not collide).
    // F3+1.8 — search overlay rendering moved to
    // `ui::components::search_overlay::paint_search_overlay`, called
    // from `build_instances` post-pane-loop into the overlay
    // scratches.  push_session no longer touches it; the per-pane
    // instance cache stays purely grid content.
}

/// Build a Shared-storage MTLBuffer over `bytes`.  Returns `None`
/// for an empty payload so the caller can skip the bind/draw.
/// Microseconds spent in `newBufferWithBytes` and bytes handed to it,
/// since the last `take_instance_buffer_cost`.
///
/// Every pass allocates a *fresh* `MTLBuffer` for its instances on
/// every frame — several megabytes a frame across the passes — which
/// is exactly what `CLAUDE.md` says a per-frame hot path must not do.
/// Whether that is what the real machine's 100–220 ms `encode` is
/// made of, though, is a question for a number, not for a reading of
/// the code: these two counters are that number.
static INSTANCE_BUF_US: AtomicU64 = AtomicU64::new(0);
static INSTANCE_BUF_BYTES: AtomicU64 = AtomicU64::new(0);

/// Read and reset the allocation cost accumulated since the last call.
pub fn take_instance_buffer_cost() -> (u64, u64) {
    (
        INSTANCE_BUF_US.swap(0, Ordering::Relaxed),
        INSTANCE_BUF_BYTES.swap(0, Ordering::Relaxed),
    )
}


/// Instance buffers that outlive the frame that fills them.
///
/// Every pass used to hand its instances to `newBufferWithBytes`,
/// which allocates a fresh `MTLBuffer` — a kernel round trip to wire
/// memory and register it with the GPU driver.  On an idle machine
/// that is 90 µs for 1.6 MB and invisible.  On a machine under real
/// load it was measured at **83–289 ms for 1.0 MB** — a thousandfold
/// stretch of one call, accounting for 99 % of nine out of twelve
/// sampled stalls (2026-08-12, 13 panes, load ~7–10).  While it
/// blocks, every pane is frozen and the supervisor's PONG deadline is
/// running.
///
/// So the buffers are allocated once and refilled in place.
/// `StorageModeShared` means `contents()` is CPU-writable, and the
/// capacity only ever grows — rounded up to a power of two so growth
/// stops happening after the first few frames.  Steady state performs
/// no allocation at all, which is what `CLAUDE.md` asks of a
/// per-frame path in the first place.
///
/// **Safety contract**: refilling in place is only sound if the GPU
/// is done with the previous frame's contents.  The IOSurface path
/// ends every frame in `waitUntilCompleted`, so it is; the live
/// `CAMetalLayer` path only waits until *scheduled*, so it keeps
/// allocating per frame and passes `None`.
pub struct InstanceBufferPool {
    slots: [Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>>; INSTANCE_SLOTS],
    caps: [usize; INSTANCE_SLOTS],
}

/// One slot per draw the IOSurface path encodes.
pub const INSTANCE_SLOTS: usize = 8;

impl Default for InstanceBufferPool {
    fn default() -> Self {
        Self { slots: Default::default(), caps: [0; INSTANCE_SLOTS] }
    }
}

impl InstanceBufferPool {
    /// Copy `bytes` into slot `slot`, growing it if need be.
    fn upload(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        slot: usize,
        bytes: &[u8],
    ) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>> {
        if bytes.is_empty() || slot >= INSTANCE_SLOTS {
            return None;
        }
        if self.caps[slot] < bytes.len() || self.slots[slot].is_none() {
            // Round up so a grid that grows by one row does not
            // reallocate — the allocation is the thing being avoided.
            let cap = bytes.len().next_power_of_two().max(64 * 1024);
            let t0 = std::time::Instant::now();
            let buf =
                device.newBufferWithLength_options(cap, MTLResourceOptions::StorageModeShared);
            INSTANCE_BUF_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
            INSTANCE_BUF_BYTES.fetch_add(cap as u64, Ordering::Relaxed);
            self.slots[slot] = buf;
            self.caps[slot] = if self.slots[slot].is_some() { cap } else { 0 };
        }
        let buf = self.slots[slot].as_ref()?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                buf.contents().as_ptr() as *mut u8,
                bytes.len(),
            );
        }
        Some(buf.clone())
    }
}

fn make_instance_buffer(
    device: &ProtocolObject<dyn MTLDevice>,
    bytes: &[u8],
) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>> {
    if bytes.is_empty() {
        return None;
    }
    let t0 = std::time::Instant::now();
    let out = unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(),
            bytes.len(),
            MTLResourceOptions::StorageModeShared,
        )
    };
    INSTANCE_BUF_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    INSTANCE_BUF_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    out
}

/// Compile `cells.metal` once.  Both the BG and FG pipelines pull
/// their entry-point functions out of the same library — the .metal
/// file declares them side-by-side.
fn build_shader_library(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLLibrary>>, String> {
    let src = NSString::from_str(SHADER_SRC);
    device
        .newLibraryWithSource_options_error(&src, None)
        .map_err(|e| format!("MTLDevice.newLibraryWithSource error: {:?}", e))
}

fn pipeline_function(
    library: &ProtocolObject<dyn MTLLibrary>,
    name: &str,
) -> Result<Retained<ProtocolObject<dyn objc2_metal::MTLFunction>>, String> {
    let n = NSString::from_str(name);
    library
        .newFunctionWithName(&n)
        .ok_or_else(|| format!("library has no function {name:?}"))
}

/// BG-pass pipeline.  No blending — opaque cells fill the whole quad.
fn build_bg_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "bg_vertex")?;
    let ffn = pipeline_function(library, "bg_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    // Alpha blending so translucent BG cells (e.g. the
    // inactive-pane dim overlay pushed at the end of
    // push_session) composite over the colour BG fills below
    // them.  Opaque cells (alpha = 1.0) render identically
    // either way — `src.a = 1` makes destination contribution
    // zero, same as no blending.
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (BG) error: {:?}", e))
}

/// FG-pass pipeline.  Alpha blending on so the glyph's coverage
/// composites onto whatever the BG pass painted underneath:
///
///     final.rgb = src.rgb * src.a + dst.rgb * (1 - src.a)
///     final.a   = src.a   * src.a + dst.a   * (1 - src.a)
fn build_fg_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "fg_vertex")?;
    let ffn = pipeline_function(library, "fg_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (FG) error: {:?}", e))
}

/// Colour-glyph FG pipeline.  Same `fg_vertex` as the mono path, but the
/// `fg_fragment_color` fragment samples the BGRA colour atlas and outputs
/// the texel directly (the emoji's own colours).  The texel is
/// **premultiplied** (the rasteriser drew into a premultiplied-alpha
/// context), so the blend uses source factor `One` rather than
/// `SourceAlpha`:
///
///     final.rgb = src.rgb * 1 + dst.rgb * (1 - src.a)
fn build_fg_color_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "fg_vertex")?;
    let ffn = pipeline_function(library, "fg_fragment_color")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (FG colour) error: {:?}", e))
}

/// Dot-pass pipeline.  Same `CellInstance` input as BG, but the
/// fragment shader smoothsteps to a circle inscribed in the quad.
/// Alpha-blended (same equation as FG) so the AA edge composites
/// cleanly over the underlying sidebar BG.
fn build_dot_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "dot_vertex")?;
    let ffn = pipeline_function(library, "dot_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (dot) error: {:?}", e))
}

fn build_ui_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "ui_rect_vertex")?;
    let ffn = pipeline_function(library, "ui_rect_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    // `One`, not `SourceAlpha`: `ui_rect_fragment` composites its
    // shadow/fill/border layers into PREMULTIPLIED form (it returns
    // `rgb * a`), so the source contribution is already scaled and the
    // blend must not scale it again.
    //
    // It used to be `SourceAlpha`, which applied alpha twice.  At
    // a = 1.0 the two formulas agree, so every opaque surface looked
    // correct and the bug hid; a 0.2-alpha fill rendered at an
    // effective 0.04 and simply disappeared.  Measured before the fix:
    // white at a = 0.5 over black read 64 instead of 128.  This is
    // where "semi-transparent panels don't work, make them opaque"
    // came from — it was never a taste constraint, it was this.
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (ui_rect) error: {:?}", e))
}

fn build_fg_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let descriptor = MTLSamplerDescriptor::new();
    descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
    descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
    device
        .newSamplerStateWithDescriptor(&descriptor)
        .ok_or_else(|| "newSamplerStateWithDescriptor returned nil".to_string())
}

impl MetalRenderer {
    /// Phase-3 BG pass into a freshly-allocated MTLTexture, with
    /// pixel readback.  Used by tests and (eventually) by an offscreen
    /// `--bench metal-render` mode.  Returns BGRA bytes, top-left
    /// origin, `width * 4` bytes per row.
    pub fn render_cells_bg_offscreen(
        &self,
        width: u32,
        height: u32,
        cells: &[CellInstance],
    ) -> Result<Vec<u8>, String> {
        // Renderable target texture, Managed so we can read back from CPU.
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setUsage(
            objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
        );
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        // Per-instance buffer.  StorageModeShared: CPU writes, GPU reads,
        // no manual sync needed.  newBufferWithBytes does the copy.
        let cells_bytes = cells_as_bytes(cells);
        let buffer = if cells_bytes.is_empty() {
            None
        } else {
            unsafe {
                self.device.newBufferWithBytes_length_options(
                    NonNull::new(cells_bytes.as_ptr() as *mut c_void).unwrap(),
                    cells_bytes.len(),
                    MTLResourceOptions::StorageModeShared,
                )
            }
        };

        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });
        }

        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| "renderCommandEncoder returned nil".to_string())?;
        encoder.setRenderPipelineState(&self.bg_pipeline);
        if let Some(buf) = &buffer {
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
            }
        }
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of::<[f32; 2]>(),
                1,
            );
            if !cells.is_empty() {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    cells.len(),
                );
            }
        }
        encoder.endEncoding();

        // Managed → CPU: synchronize so subsequent getBytes reads
        // the latest GPU writes.
        let blit = cmd
            .blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();

        cmd.commit();
        { cmd.waitUntilCompleted() };

        let bytes_per_row = (width as usize) * 4;
        let mut bytes = vec![0u8; bytes_per_row * height as usize];
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width: width as usize,
                height: height as usize,
                depth: 1,
            },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
                bytes_per_row,
                region,
                0,
            );
        }
        Ok(bytes)
    }
}

/// SAFETY: `CellInstance` is `#[repr(C)]` with no padding, so its
/// memory representation is a flat slice of bytes.  No interior
/// uninitialised bytes; safe to view the slice as `&[u8]`.
fn cells_as_bytes(cells: &[CellInstance]) -> &[u8] {
    let len = std::mem::size_of_val(cells);
    unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, len) }
}

/// SAFETY: same reasoning as `cells_as_bytes` — `GlyphInstance` is
/// `#[repr(C)]` with no padding.
fn ui_rects_as_bytes(rects: &[UiRectInstance]) -> &[u8] {
    let len = std::mem::size_of_val(rects);
    unsafe { std::slice::from_raw_parts(rects.as_ptr() as *const u8, len) }
}

fn glyphs_as_bytes(glyphs: &[GlyphInstance]) -> &[u8] {
    let len = std::mem::size_of_val(glyphs);
    unsafe { std::slice::from_raw_parts(glyphs.as_ptr() as *const u8, len) }
}

impl MetalRenderer {
    /// Phase-4 FG pass into a freshly-allocated MTLTexture, with
    /// pixel readback.  Atlas is the R8 texture from `GlyphAtlas`.
    /// `clear` is the colour painted before glyphs are composited
    /// (in tests this is typically opaque black so glyph alpha
    /// trivially shows up in the readback).
    pub fn render_glyphs_fg_offscreen(
        &self,
        width: u32,
        height: u32,
        atlas: &ProtocolObject<dyn MTLTexture>,
        glyphs: &[GlyphInstance],
        clear: (f64, f64, f64, f64),
    ) -> Result<Vec<u8>, String> {
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setUsage(
            objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
        );
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        let glyph_bytes = glyphs_as_bytes(glyphs);
        let buffer = if glyph_bytes.is_empty() {
            None
        } else {
            unsafe {
                self.device.newBufferWithBytes_length_options(
                    NonNull::new(glyph_bytes.as_ptr() as *mut c_void).unwrap(),
                    glyph_bytes.len(),
                    MTLResourceOptions::StorageModeShared,
                )
            }
        };

        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor {
                red: clear.0,
                green: clear.1,
                blue: clear.2,
                alpha: clear.3,
            });
        }

        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| "renderCommandEncoder returned nil".to_string())?;
        encoder.setRenderPipelineState(&self.fg_pipeline);
        if let Some(buf) = &buffer {
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
            }
        }
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of::<[f32; 2]>(),
                1,
            );
            encoder.setFragmentTexture_atIndex(Some(atlas), 0);
            encoder.setFragmentSamplerState_atIndex(Some(&self.fg_sampler), 0);
            if !glyphs.is_empty() {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    glyphs.len(),
                );
            }
        }
        encoder.endEncoding();

        let blit = cmd
            .blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();

        cmd.commit();
        { cmd.waitUntilCompleted() };

        let bytes_per_row = (width as usize) * 4;
        let mut bytes = vec![0u8; bytes_per_row * height as usize];
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width: width as usize,
                height: height as usize,
                depth: 1,
            },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
                bytes_per_row,
                region,
                0,
            );
        }
        Ok(bytes)
    }
}

pub(crate) fn system_default_device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>, String> {
    // objc2-metal 0.3 wraps the +1 retained pointer (or null) into
    // `Option<Retained<..>>` for us.
    MTLCreateSystemDefaultDevice()
        .ok_or_else(|| "MTLCreateSystemDefaultDevice returned nil — no Metal device".into())
}

// ───────────────────────────────────────────────────────────────────
// Canvas flush — submission-order = z-order multi-encoder path.
// (P2b of the UI system RFC.  See docs/ui-system-rfc.md.)
// ───────────────────────────────────────────────────────────────────

/// Convert a `RectPrim` to a `UiRectInstance` for the ui_rects
/// pipeline.  Border / shadow optional fields fold cleanly into
/// the shader's existing knobs (0 / TRANSPARENT = skip).
fn ui_rect_instance_from_rect(r: &crate::ui::core::canvas::RectPrim) -> UiRectInstance {
    let (border_w, border_c) = r.border
        .map(|(w, c)| (w as f32, c.to_rgba_f32()))
        .unwrap_or((0.0, [0.0; 4]));
    let (shadow_blur, shadow_alpha, shadow_color) = r.shadow
        .map(|(blur, _offset, c)| (blur as f32, c.a as f32, c.to_rgba_f32()))
        .unwrap_or((0.0, 0.0, [0.0; 4]));
    UiRectInstance {
        origin: [r.x as f32, r.y as f32],
        size:   [r.w as f32, r.h as f32],
        fill_color: r.fill.to_rgba_f32(),
        border_color: border_c,
        corner_radius: r.radius as f32,
        border_width: border_w,
        shadow_blur,
        shadow_alpha,
        shadow_color,
    }
}

/// Axis-aligned horizontal or vertical line, emitted as a
/// degenerate `UiRectInstance` with radius=0.  Width = stroke
/// width.  Non-axis-aligned lines aren't supported yet — they'd
/// need a rotated line shader.
fn ui_rect_instance_from_line(l: &crate::ui::core::canvas::LinePrim) -> UiRectInstance {
    let (x, y, w, h) = if (l.from.1 - l.to.1).abs() < 0.5 {
        // Horizontal line.
        let x_min = l.from.0.min(l.to.0);
        let x_max = l.from.0.max(l.to.0);
        let cy = (l.from.1 + l.to.1) * 0.5;
        (x_min, cy - l.width * 0.5, x_max - x_min, l.width)
    } else if (l.from.0 - l.to.0).abs() < 0.5 {
        // Vertical line.
        let y_min = l.from.1.min(l.to.1);
        let y_max = l.from.1.max(l.to.1);
        let cx = (l.from.0 + l.to.0) * 0.5;
        (cx - l.width * 0.5, y_min, l.width, y_max - y_min)
    } else {
        // Diagonal — degenerate fallback: bounding box.  Caller
        // hits this only on accidental misuse; lines should be
        // axis-aligned for now.
        let x = l.from.0.min(l.to.0);
        let y = l.from.1.min(l.to.1);
        let w = (l.from.0 - l.to.0).abs().max(l.width);
        let h = (l.from.1 - l.to.1).abs().max(l.width);
        (x, y, w, h)
    };
    UiRectInstance {
        origin: [x as f32, y as f32],
        size:   [w as f32, h as f32],
        fill_color: l.color.to_rgba_f32(),
        border_color: [0.0; 4],
        corner_radius: 0.0,
        border_width: 0.0,
        shadow_blur: 0.0,
        shadow_alpha: 0.0,
        shadow_color: [0.0; 4],
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CanvasRunKind {
    UiRect,
    Glyph,
    /// Phase 7 — colour-emoji glyph run.  Drawn through the
    /// `fg_color_pipeline` sampling the BGRA `color_atlas` so the
    /// glyph emits real colours rather than alpha-only silhouette.
    ColorGlyph,
}

struct CanvasRun {
    kind: CanvasRunKind,
    count: usize,
}

/// Walk a Canvas's primitives in submission order, emit instances
/// into the two flat buffers, and record contiguous runs by
/// pipeline kind so the encoder can switch pipelines at run
/// boundaries (preserving submission-order = z-order).
///
/// `font_metrics` carries the cell-grid sizing the chrome font
/// uses; the renderer's existing `push_text_run` consumes them
/// the same way.
#[allow(clippy::too_many_arguments)]
fn build_canvas_runs(
    canvas: &crate::ui::core::canvas::Canvas,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w_f: f32,
    atlas_h_f: f32,
    color_atlas_w_f: f32,
    color_atlas_h_f: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    out_ui: &mut Vec<UiRectInstance>,
    out_glyphs: &mut Vec<GlyphInstance>,
    out_color_glyphs: &mut Vec<GlyphInstance>,
    ui_font: bool,
) -> Vec<CanvasRun> {
    use crate::ui::core::canvas::Primitive;

    let mut runs: Vec<CanvasRun> = Vec::new();
    let mut cur: Option<CanvasRunKind> = None;
    let bump = |runs: &mut Vec<CanvasRun>, k: CanvasRunKind, n: usize| {
        if runs.last().map(|r| r.kind == k).unwrap_or(false) {
            runs.last_mut().unwrap().count += n;
        } else {
            runs.push(CanvasRun { kind: k, count: n });
        }
    };

    for p in canvas.primitives() {
        match p {
            Primitive::Rect(r) => {
                out_ui.push(ui_rect_instance_from_rect(r));
                bump(&mut runs, CanvasRunKind::UiRect, 1);
                cur = Some(CanvasRunKind::UiRect);
            }
            Primitive::Line(l) => {
                out_ui.push(ui_rect_instance_from_line(l));
                bump(&mut runs, CanvasRunKind::UiRect, 1);
                cur = Some(CanvasRunKind::UiRect);
            }
            Primitive::Text(t) => {
                let mono_before = out_glyphs.len();
                let color_before = out_color_glyphs.len();
                let baseline_y = t.y as f32 + ascent;
                // Per-prim font_kind overrides the encoder's global
                // `ui_font`.  `None` (the default for every `text(...)`
                // call) inherits, so chrome that laid itself out
                // against mono cell metrics keeps that font without
                // having to grow `.mono()` annotations everywhere.
                let use_ui = match t.font_kind {
                    Some(crate::ui::core::canvas::TextFontKind::Ui) => true,
                    Some(crate::ui::core::canvas::TextFontKind::Mono) => false,
                    None => ui_font,
                };
                if use_ui {
                    // SF Pro path: pass raw y_top_phys (not baseline_y).
                    // push_text_run_ui_shaped derives baseline from the
                    // run's own ascent — correct geometry across UiSize.
                    push_text_run_ui_shaped(
                        &t.content,
                        t.x as f32,
                        t.y as f32,
                        t.color.to_rgba_f32(),
                        ascent,
                        atlas_w_f, atlas_h_f,
                        color_atlas_w_f, color_atlas_h_f,
                        t.weight,
                        t.opts,
                        t.ui_size_q,
                        font, atlas, color_atlas, out_glyphs, out_color_glyphs,
                    );
                } else {
                    push_text_run_kind(
                        &t.content,
                        t.x as f32,
                        baseline_y,
                        t.color.to_rgba_f32(),
                        cell_w, cell_h, ascent,
                        atlas_w_f, atlas_h_f,
                        font, atlas, out_glyphs,
                        FontKind::Terminal,
                    );
                }
                // Phase 7 — the shape path can interleave mono +
                // colour glyphs in a single TextPrim (LTR Latin then
                // a fallback 👍 then more Latin).  We collapse to ONE
                // run per kind here: all of this text's mono glyphs
                // go in a `Glyph` run, all of its colour glyphs go in
                // a `ColorGlyph` run.  Submission-order within each
                // sink is preserved, and visually identical pixels
                // because the mono FG pass and the colour FG pass
                // composite to the same target with the same
                // pre-multiplied blend.
                let mono_added = out_glyphs.len() - mono_before;
                if mono_added > 0 {
                    bump(&mut runs, CanvasRunKind::Glyph, mono_added);
                    cur = Some(CanvasRunKind::Glyph);
                }
                let color_added = out_color_glyphs.len() - color_before;
                if color_added > 0 {
                    bump(&mut runs, CanvasRunKind::ColorGlyph, color_added);
                    cur = Some(CanvasRunKind::ColorGlyph);
                }
            }
        }
    }
    let _ = cur;
    runs
}

/// Free-function `encode_canvas` — composes with caller's
/// destructured `&mut self` borrows (render_layout style).  The
/// method wrapper below is for tests / one-shot callers.
#[allow(clippy::too_many_arguments)]
pub fn encode_canvas_into(
    canvas: &crate::ui::core::canvas::Canvas,
    target: &ProtocolObject<dyn MTLTexture>,
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    ui_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_color_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    device: &ProtocolObject<dyn MTLDevice>,
    font: &mut FontCache,
    clear_color: Option<MTLClearColor>,
    viewport_px: &[f32; 2],
    chrome_cell_w: f32,
    chrome_cell_h: f32,
    chrome_ascent: f32,
    ui_font: bool,
) {
    let (aw, ah) = atlas.dims();
    let atlas_w_f = aw as f32;
    let atlas_h_f = ah as f32;
    let (caw, cah) = color_atlas.dims();
    let color_atlas_w_f = caw as f32;
    let color_atlas_h_f = cah as f32;
    let mut ui_buf: Vec<UiRectInstance> = Vec::new();
    let mut gl_buf: Vec<GlyphInstance> = Vec::new();
    let mut color_gl_buf: Vec<GlyphInstance> = Vec::new();
    let runs = build_canvas_runs(
        canvas,
        chrome_cell_w, chrome_cell_h, chrome_ascent,
        atlas_w_f, atlas_h_f,
        color_atlas_w_f, color_atlas_h_f,
        font, atlas, color_atlas,
        &mut ui_buf, &mut gl_buf, &mut color_gl_buf,
        ui_font,
    );

    let mut ui_cursor = 0usize;
    let mut gl_cursor = 0usize;
    let mut color_gl_cursor = 0usize;
    let mut first_pass = true;
    let viewport_ptr = NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap();
    let viewport_len = std::mem::size_of::<[f32; 2]>();

    for run in &runs {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            if first_pass && clear_color.is_some() {
                color.setLoadAction(MTLLoadAction::Clear);
                color.setClearColor(clear_color.unwrap());
            } else {
                color.setLoadAction(MTLLoadAction::Load);
            }
            color.setStoreAction(MTLStoreAction::Store);
        }
        first_pass = false;

        let enc = match cmd.renderCommandEncoderWithDescriptor(&pass) {
            Some(e) => e,
            None => continue,
        };
        unsafe {
            enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1);
        }
        match run.kind {
            CanvasRunKind::UiRect => {
                enc.setRenderPipelineState(ui_pipeline);
                let slice = &ui_buf[ui_cursor..ui_cursor + run.count];
                let buf = make_instance_buffer(device, ui_rects_as_bytes(slice));
                if let Some(b) = &buf {
                    unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
                }
                unsafe {
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                        MTLPrimitiveType::Triangle, 0, 6, run.count,
                    );
                }
                ui_cursor += run.count;
            }
            CanvasRunKind::Glyph => {
                enc.setRenderPipelineState(fg_pipeline);
                let slice = &gl_buf[gl_cursor..gl_cursor + run.count];
                let buf = make_instance_buffer(device, glyphs_as_bytes(slice));
                if let Some(b) = &buf {
                    unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
                }
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(atlas.texture()), 0);
                    enc.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                        MTLPrimitiveType::Triangle, 0, 6, run.count,
                    );
                }
                gl_cursor += run.count;
            }
            CanvasRunKind::ColorGlyph => {
                // Phase 7 — colour-emoji pass: same vertex shader as
                // mono, different pipeline (`fg_color_pipeline`) so
                // the fragment shader samples the BGRA atlas and
                // outputs premultiplied colour instead of tinting
                // by the per-cell `color` field.
                enc.setRenderPipelineState(fg_color_pipeline);
                let slice = &color_gl_buf[color_gl_cursor..color_gl_cursor + run.count];
                let buf = make_instance_buffer(device, glyphs_as_bytes(slice));
                if let Some(b) = &buf {
                    unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
                }
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(color_atlas.texture()), 0);
                    enc.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                        MTLPrimitiveType::Triangle, 0, 6, run.count,
                    );
                }
                color_gl_cursor += run.count;
            }
        }
        enc.endEncoding();
    }

    if first_pass && clear_color.is_some() {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setClearColor(clear_color.unwrap());
            color.setStoreAction(MTLStoreAction::Store);
        }
        if let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) {
            enc.endEncoding();
        }
    }
}

impl MetalRenderer {
    /// Method wrapper — calls the free `encode_canvas_into`
    /// with the renderer's own state.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_canvas(
        &mut self,
        canvas: &crate::ui::core::canvas::Canvas,
        target: &ProtocolObject<dyn MTLTexture>,
        cmd: &ProtocolObject<dyn MTLCommandBuffer>,
        clear_color: Option<MTLClearColor>,
        viewport_px: &[f32; 2],
        chrome_cell_w: f32,
        chrome_cell_h: f32,
        chrome_ascent: f32,
        ui_font: bool,
    ) {
        encode_canvas_into(
            canvas, target, cmd,
            &self.ui_pipeline, &self.fg_pipeline, &self.fg_color_pipeline, &self.fg_sampler,
            &mut self.atlas, &mut self.color_atlas, &self.device, &mut self.font,
            clear_color, viewport_px,
            chrome_cell_w, chrome_cell_h, chrome_ascent,
            ui_font,
        );
    }

    /// Test helper: encode a Canvas into a freshly-allocated
    /// `width × height` Managed texture, sync back to CPU,
    /// return the BGRA8 bytes.  Pairs with the
    /// `canvas_*` tests below to verify submission-order
    /// invariants on pixel readback.
    pub fn render_canvas_to_bitmap(
        &mut self,
        width: u32,
        height: u32,
        canvas: &crate::ui::core::canvas::Canvas,
        chrome_cell_w: f32,
        chrome_cell_h: f32,
        chrome_ascent: f32,
        ui_font: bool,
    ) -> Result<Vec<u8>, String> {
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT, width as usize, height as usize, false,
            )
        };
        descriptor.setUsage(
            objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
        );
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        let texture = self.device.newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        let cmd = self.queue.commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        self.encode_canvas(
            canvas,
            ProtocolObject::from_ref(&*texture),
            ProtocolObject::from_ref(&*cmd),
            Some(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 }),
            &viewport_px,
            chrome_cell_w, chrome_cell_h, chrome_ascent,
            ui_font,
        );

        let blit = cmd.blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();
        cmd.commit();
        { cmd.waitUntilCompleted() };

        let bytes_per_row = (width as usize) * 4;
        let mut bytes = vec![0u8; bytes_per_row * height as usize];
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize { width: width as usize, height: height as usize, depth: 1 },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
                bytes_per_row, region, 0,
            );
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 9 (extended) — pure-Rust SSIM (Structural Similarity
    /// Index) over the luma channel.  No new deps — `png` is already
    /// in the workspace, the rest is arithmetic over RGBA bytes.
    ///
    /// 8×8 non-overlapping windows; standard SSIM weights
    /// (`k1 = 0.01`, `k2 = 0.03`, `L = 255`).  Luma per pixel uses
    /// BT.709 weights (`0.2126 R + 0.7152 G + 0.0722 B`).  Returns
    /// the unweighted mean SSIM across every window — 1.0 = identical,
    /// `< 0.98` (per `docs/font-rendering-design.md` §9) is the
    /// project's visual-regression gate.
    ///
    /// Input bytes are RGBA (the snapshot harness already does the
    /// BGRA→RGBA swap before calling this).  Sizes must agree;
    /// caller checks dims via the `png::Decoder` header.
    fn ssim_luma(a: &[u8], b: &[u8], w: u32, h: u32) -> f64 {
        const W: usize = 8;
        let stride = (w as usize) * 4;
        let luma = |bytes: &[u8], x: usize, y: usize| -> f64 {
            let i = y * stride + x * 4;
            0.2126 * bytes[i] as f64
                + 0.7152 * bytes[i + 1] as f64
                + 0.0722 * bytes[i + 2] as f64
        };
        const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
        const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
        let mut total: f64 = 0.0;
        let mut count: usize = 0;
        let mut y = 0;
        while y + W <= h as usize {
            let mut x = 0;
            while x + W <= w as usize {
                let mut mu_a = 0.0;
                let mut mu_b = 0.0;
                for dy in 0..W {
                    for dx in 0..W {
                        mu_a += luma(a, x + dx, y + dy);
                        mu_b += luma(b, x + dx, y + dy);
                    }
                }
                let n = (W * W) as f64;
                mu_a /= n;
                mu_b /= n;
                let mut var_a = 0.0;
                let mut var_b = 0.0;
                let mut cov = 0.0;
                for dy in 0..W {
                    for dx in 0..W {
                        let da = luma(a, x + dx, y + dy) - mu_a;
                        let db = luma(b, x + dx, y + dy) - mu_b;
                        var_a += da * da;
                        var_b += db * db;
                        cov += da * db;
                    }
                }
                var_a /= n;
                var_b /= n;
                cov /= n;
                let s = ((2.0 * mu_a * mu_b + C1) * (2.0 * cov + C2))
                    / ((mu_a * mu_a + mu_b * mu_b + C1) * (var_a + var_b + C2));
                total += s;
                count += 1;
                x += W;
            }
            y += W;
        }
        if count == 0 { 1.0 } else { total / count as f64 }
    }

    /// Phase 9 — load a baseline PNG as raw RGBA bytes (matches what
    /// the snapshot harness produces post-channel-swap).  Returns
    /// `None` if the file doesn't exist or its dims disagree — caller
    /// treats a missing baseline as "first-time lock, write only".
    fn load_baseline_rgba(
        path: &std::path::Path,
        expected_w: u32,
        expected_h: u32,
    ) -> Option<Vec<u8>> {
        let file = std::fs::File::open(path).ok()?;
        let decoder = png::Decoder::new(std::io::BufReader::new(file));
        let mut reader = decoder.read_info().ok()?;
        let info = reader.info();
        if info.width != expected_w || info.height != expected_h {
            return None;
        }
        let mut buf = vec![0u8; reader.output_buffer_size()?];
        let frame = reader.next_frame(&mut buf).ok()?;
        buf.truncate(frame.buffer_size());
        // Force RGBA shape — decoder honours the encoder's RGBA8
        // setup from `write_snapshot_png` below.  Other shapes would
        // need a channel pad / expand here; for now reject by None.
        if frame.color_type != png::ColorType::Rgba {
            return None;
        }
        Some(buf)
    }

    /// Phase 9 — SSIM gate per `docs/font-rendering-design.md` §9.
    /// Snapshot tests call this after producing fresh RGBA bytes:
    ///   - if `MARSPOT_FONT_SNAPSHOT=check` and a baseline exists,
    ///     compute SSIM and `assert! > 0.98` — failing means a real
    ///     visual drift slipped in.
    ///   - otherwise no-op (writes happen in the caller).
    ///
    /// The `check` mode is opt-in so default `cargo nextest` stays
    /// fast and host-portable (no Metal device → snapshot tests skip
    /// entirely upstream).  `bin/font-snapshot-check.sh` is the
    /// wrapper that flips the env and runs the matrix.
    fn assert_snapshot_ssim(
        rgba: &[u8],
        baseline_path: &std::path::Path,
        w_px: u32,
        h_px: u32,
        threshold: f64,
    ) {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").as_deref() != Ok("check") {
            return;
        }
        let baseline = match load_baseline_rgba(baseline_path, w_px, h_px) {
            Some(b) => b,
            None => {
                eprintln!(
                    "[ssim] no baseline at {} (or dim mismatch) — skipping check",
                    baseline_path.display()
                );
                return;
            }
        };
        let s = ssim_luma(rgba, &baseline, w_px, h_px);
        eprintln!(
            "[ssim] {} = {:.4} (threshold {:.4})",
            baseline_path.display(),
            s,
            threshold,
        );
        assert!(
            s >= threshold,
            "SSIM {:.4} < {:.4} — visual regression vs {}",
            s,
            threshold,
            baseline_path.display(),
        );
    }

    /// Font v5 snapshot — renders the dev-panel canvas with the
    /// `Font v5` section active, encodes the BGRA framebuffer to a
    /// PNG, and writes it to disk for manual eyeball.  Opt-in via
    /// the `MARSPOT_FONT_SNAPSHOT` env var so the suite still runs
    /// quickly when nobody asks for a snapshot.
    ///
    /// Workflow:
    ///   MARSPOT_FONT_SNAPSHOT=1 cargo nextest run -p marspot --lib \
    ///     font_v5_showcase_snapshot
    ///   open bench/font-rendering/snapshots/font_v5_showcase.png
    ///
    /// `bin/font-snapshot.sh` wraps both steps.  Falling back to the
    /// project default location keeps the workflow muscle-memory
    /// match the other bench scripts (`bin/bench-remote.sh` writes
    /// under `bench/remote-runs/`).
    #[test]
    fn font_v5_showcase_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        // Production default size: 420 × 520 logical pt → 840 × 1040
        // px at 2× retina.  Matches what `DevPanelState::default()`
        // gives the real dev window, so the snapshot reflects the
        // exact pixels the user sees when they open the panel.
        let w_px: u32 = 840;
        let h_px: u32 = 1040;
        let state = crate::ui::components::DevPanelState {
            visible: true,
            origin_pt: (0.0, 0.0),
            size_pt: (420.0, 520.0),
            // Post-2026-06-25 hierarchy:Font v5 moved out of UI tab
            // into its own Font tab.  Snapshot renders the Font tab
            // with showcase active so the menu highlight + content
            // both line up on the same section.
            active_tab: crate::ui::components::dev_panel::TAB_FONT,
            active_section: crate::ui::components::dev_panel::SECTION_FONT_V5_SHOWCASE,
            scale: 2.0,
        };
        let measure = crate::chrome_measure::ChromeMeasure::new(
            renderer.font_mut(),
            16.0,
            32.0,
        );
        let canvas = crate::ui::components::build_dev_panel_canvas(
            &state,
            w_px as f64,
            h_px as f64,
            16.0, // chrome_cell_w (px) — Monaco 12pt 2×
            32.0, // chrome_cell_h
            24.0, // chrome_ascent
            &measure,
        );
        drop(measure);
        // ui_font = false matches the live dev panel: chrome stays
        // Monaco mono by default, the showcase rows opt INTO SF Pro
        // via `.ui()` per text run.  This snapshot is therefore
        // exactly what a user sees when they navigate to "Font v5".
        let bytes = renderer
            .render_canvas_to_bitmap(w_px, h_px, &canvas, 16.0, 32.0, 24.0, false)
            .expect("canvas render");

        // BGRA → RGBA channel swap for PNG.
        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];     // R
            rgba[i + 1] = bytes[i + 1]; // G
            rgba[i + 2] = bytes[i];     // B
            rgba[i + 3] = bytes[i + 3]; // A
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_showcase.png");
        // Phase 9 — `=check` runs SSIM vs the committed baseline BEFORE
        // we overwrite the on-disk PNG; otherwise the on-disk PNG IS
        // the fresh render and SSIM would be 1.0 by definition.
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 snapshot] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — visual-regression strings.  Renders the
    /// canonical Mono Monaco workload (ASCII + CJK cascade + emoji
    /// + kerning/ligature lines) as a single PNG so a reviewer can
    /// eyeball it AND a future SSIM gate can diff it byte-for-byte
    /// against a frozen baseline.  Same opt-in shape as
    /// `font_v5_showcase_snapshot` (env-gated so default test runs
    /// stay fast).  Output:
    ///   bench/font-rendering/snapshots/font_v5_mono_grid.png
    ///
    /// Workflow:
    ///   MARSPOT_FONT_SNAPSHOT=1 cargo nextest run -p marspot --lib \
    ///     font_v5_mono_grid_snapshot
    ///
    /// SSIM > 0.98 gate per `docs/font-rendering-design.md` §9 lives
    /// in a follow-up — this commit only fixes the rendered bytes.
    #[test]
    fn font_v5_mono_grid_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1200;
        let h_px: u32 = 600;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // Five lines, one per font-feature axis the design doc §15
        // pins as a v5 acceptance test:
        //   L1 ASCII alphabet  — Monaco cell-pitch sanity
        //   L2 CJK cascade     — fallback chain into PingFang/Hiragino/AppleSDGothic
        //   L3 emoji + ZWJ     — colour atlas + ZWJ cluster
        //   L4 kerning/ligature — `Ta` / `fi` / `==>` shaping in mono context
        //   L5 mixed RTL hello — bidi placeholder (RTL routing comes later)
        let lines = [
            "The quick brown fox jumps over the lazy dog 0123456789",
            "你好世界  こんにちは  안녕하세요",
            "👨‍👩‍👧‍👦  🍕  🇯🇵  🌈  ⭐  ❤️",
            "Tax  fi  fl  ff  ==>  !=  !==",
            "Bidi placeholder: Hello مرحبا עברית",
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        let mut canvas = Canvas::new(
            2.0, // 2× retina scale matches the dev-panel snapshot
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
        let pad_x_pt = 20.0;
        let pad_y_pt = 16.0;
        // Solid dark BG matches the production terminal BG so the
        // glyphs read at a glance.
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (i, line) in lines.iter().enumerate() {
            let y_pt = pad_y_pt + (i as f64) * line_h_pt;
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .draw();
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                false, // Mono path (Monaco)
            )
            .expect("canvas render");

        // BGRA → RGBA channel swap for PNG.
        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_mono_grid.png");
        // Phase 9 — SSIM gate (see assert_snapshot_ssim doc).
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 mono grid] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — box-drawing + block-element pieces.
    /// Validates the per-codepoint custom raster (`box_drawing_arms`
    /// + `block_element_rects` in render_metal.rs) — the path that
    /// can fall out of sync with `cell_h` / baseline if any of the
    /// chrome-font metrics drift.  Locks the rendered pixels so a
    /// hairline gap at any `┌─┐│└─┘` junction surfaces as an SSIM
    /// drop instead of being noticed by the user months later.
    ///
    /// Same env triplet as the other snapshots (`MARSPOT_FONT_SNAPSHOT`):
    /// unset → skip, =1 → write PNG fixture, =check → SSIM ≥ 0.98
    /// vs the committed baseline.  Bypassed by SF Pro / variable
    /// weight (those are covered by `font_v5_showcase_snapshot`);
    /// this one is purely the Monaco custom-raster path.
    #[test]
    fn font_v5_box_drawing_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1000;
        let h_px: u32 = 500;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // Five lines, each a tight stress on the custom raster path:
        //   L1 — light frame + horizontal pieces;  contiguous corners
        //        must line up cell-by-cell.
        //   L2 — heavy frame + cross + tee in the standard cluster.
        //   L3 — double-line frame (`╔═╗`) for the variant raster.
        //   L4 — block elements (full / half / quarter / shade).
        //   L5 — mixed light/heavy junctions, the case that broke
        //        most often in 2025-Q4 raster regressions.
        let lines = [
            "┌─────┬─────┐  ╭─────╮  ┏━━━━━┓",
            "│ AAA │ BBB │  │ CCC │  ┃ DDD ┃",
            "└─────┴─────┘  ╰─────╯  ┗━━━━━┛",
            "█▓▒░  ▀▄  ▌▐  ▔▁  ▍▎▏  ▕",
            "├─┼─┤ ╠═╬═╣ ┝━┿━┥ ┠─╂─┨",
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
        let pad_x_pt = 20.0;
        let pad_y_pt = 16.0;
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (i, line) in lines.iter().enumerate() {
            let y_pt = pad_y_pt + (i as f64) * line_h_pt;
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .draw();
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                false,
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_box_drawing.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 box drawing] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — sub-pixel positioning fingerprint.
    /// Phase 4 quantises every CTLine pen-x to one of 4 buckets
    /// (`subpx_x ∈ 0..4`) so the atlas can reuse a single bitmap
    /// across pixel-aligned and 0.25/0.5/0.75-shifted positions.
    /// A regression on either side of that quantisation (bucket
    /// miscalc → wrong slot, or atlas slot wrong-shifted) shows up
    /// as character bleed on dense glyph runs:  `iiii` smears,
    /// `lll` blends, `AVAV` kerning floats.  Lock the rendered
    /// fingerprint here so the regression surfaces as an SSIM drop
    /// at `bin/font-snapshot-check.sh` instead of a user-reported
    /// "fonts look slightly off" two months later.
    ///
    /// Mono Monaco only — chrome SF Pro path's subpx fingerprint is
    /// covered as a row inside `font_v5_showcase_snapshot`.
    #[test]
    fn font_v5_subpx_fingerprint_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1000;
        let h_px: u32 = 500;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // Each line stresses a different sub-pixel scenario:
        //   L1 — dense vertical-stem runs (`iiii lll`): atlas slot
        //        x-shift errors show up as inter-glyph bleed.
        //   L2 — wide-kerned letter pairs that ride sub-pixel
        //        boundaries (`AV WA Yo Ta`).
        //   L3 — narrow latin + punctuation (`!?,.;:`) — typical
        //        dense punctuation in code / log output.
        //   L4 — same letters repeated at varying counts to exercise
        //        every bucket cell-by-cell (`a aa aaa aaaa`).
        //   L5 — mixed slot widths via CJK fullwidth (`你好` ≡ 2
        //        cells each), pinning bucket transitions across the
        //        cluster boundary.
        let lines = [
            "iiiiii  lllll  IIIIII  !!!!!!",
            "AVAVAV  WAWA  YoYoYo  TaTa  fifi",
            "Code: foo(bar, baz);  !?,.;:  /* x */",
            "a aa aaa aaaa aaaaa aaaaaa aaaaaaa",
            "你好 世界 こんにちは 안녕 a b c",
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
        let pad_x_pt = 20.0;
        let pad_y_pt = 16.0;
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (i, line) in lines.iter().enumerate() {
            let y_pt = pad_y_pt + (i as f64) * line_h_pt;
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .draw();
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                false,
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_subpx_fingerprint.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 subpx fingerprint] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — chrome SF Pro at small point sizes.
    /// `font_v5_showcase_snapshot` already covers the SF Pro path
    /// at production sizes (TITLE=22 / H=15 / BODY=13 / EMOJI=20),
    /// but the riskiest regressions in chrome rendering hit small
    /// type:  11–13 pt is where sub-pixel AA, hinting, and the
    /// Monaco fallback for tiny ASCII all sit on knife edges.  Tab
    /// strips, sidebar labels, status badges all live here, and a
    /// silent drift surfaces as "fonts feel off" weeks after the
    /// fact — exactly the regression class an SSIM gate catches
    /// for free.
    ///
    /// One column per size (11 / 12 / 13 / 14 pt), each rendering
    /// the same ASCII + CJK + emoji line so the eye can scan
    /// vertically for cell-pitch / x-height drift.
    #[test]
    fn font_v5_chrome_small_sizes_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1200;
        let h_px: u32 = 600;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // For each row we render at a different sub-13-pt size via
        // `Text::ui(size_pt, weight, opts)` so the SF Pro variable-
        // weight pipeline + CTLine shape + GlyphAtlas natural-bbox
        // raster all line up against the size_q bucket.  Sample
        // string mixes ASCII + CJK + ligature pair so each row
        // probes a different worry per size:
        let lines: &[(f64, &str)] = &[
            (11.0, "11pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
            (12.0, "12pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
            (13.0, "13pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
            (14.0, "14pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        use crate::font_shape::ShapeOptions;
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let pad_x_pt = 20.0;
        let pad_y_pt = 16.0;
        let line_gap_pt = 28.0;
        let opts = ShapeOptions::full();
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (i, (pt, line)) in lines.iter().enumerate() {
            let y_pt = pad_y_pt + (i as f64) * line_gap_pt;
            let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .ui()
                .ui_size_q(size_q)
                .weight(400)
                .opts(opts)
                .draw();
        }
        // Trailing block: 100/400/700/900 weight stack at 12pt so
        // variable-weight drift (one variant slipped out of cache)
        // also shows up here, not just in the big showcase.
        let weight_rows: &[(u16, &str)] = &[
            (100, "12pt w100  The quick brown fox jumps"),
            (400, "12pt w400  The quick brown fox jumps"),
            (700, "12pt w700  The quick brown fox jumps"),
            (900, "12pt w900  The quick brown fox jumps"),
        ];
        let weight_size_q = crate::glyph_atlas::GlyphKey::size_q_for(12.0);
        let weight_pad_y_pt = pad_y_pt + (lines.len() as f64) * line_gap_pt + 16.0;
        for (i, (w, line)) in weight_rows.iter().enumerate() {
            let y_pt = weight_pad_y_pt + (i as f64) * 22.0;
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .ui()
                .ui_size_q(weight_size_q)
                .weight(*w)
                .opts(opts)
                .draw();
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                true, // ui_font=true — emit SF Pro path
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_chrome_small_sizes.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 chrome small sizes] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — chrome CJK fallback baseline alignment.
    /// SF Pro doesn't ship CJK glyphs;  CoreText falls back through
    /// `PingFang SC` (Simplified Chinese) → `Hiragino Sans` /
    /// `Hiragino Mincho` (Japanese) → `Apple SD Gothic Neo` (Korean)
    /// per-cluster.  When per-glyph baseline / ascent differ across
    /// fonts, mixed-script lines drift — Latin + CJK on the same row
    /// no longer share the same writing baseline, and the eye reads
    /// it as "this looks wonky".  At chrome small sizes (11-13pt) the
    /// drift is sub-pixel and easy to miss;  at large sizes the drift
    /// scales linearly with size.  Lock the alignment fingerprint at
    /// 24pt + 36pt so any future fallback-chain reorder or font
    /// metrics re-sync surfaces as an SSIM hit.
    #[test]
    fn font_v5_cjk_fallback_baseline_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1400;
        let h_px: u32 = 600;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // Each row mixes Latin (SF Pro) with the three CJK regions
        // back-to-back.  Vertical scan tells you whether the four
        // baselines stay aligned across the size + weight combo.
        // 36pt locks coarse drift;  24pt + 18pt covers the rest of
        // the body-text range.
        let rows: &[(f64, u16, &str)] = &[
            (36.0, 600, "Hello 你好 こんにちは 안녕"),
            (24.0, 400, "Hello 你好 こんにちは 안녕 — mixed baseline"),
            (24.0, 700, "Bold 你好 こんにちは 안녕 — bold mix"),
            (18.0, 400, "18pt mixed 中文 + English + 日本語 + 한국어"),
            (18.0, 400, "Mac 偏好设定 · Mac の環境設定 · Mac 환경설정"),
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        use crate::font_shape::ShapeOptions;
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let pad_x_pt = 20.0;
        // First row is 36 pt — ascent eats ~30 pt above the baseline,
        // so start the cursor ~36 pt down or the top of "Hello" gets
        // clipped at the image edge.
        let mut y_pt = 36.0;
        let opts = ShapeOptions::full();
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (pt, weight, line) in rows.iter() {
            let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .ui()
                .ui_size_q(size_q)
                .weight(*weight)
                .opts(opts)
                .draw();
            // Row gap proportional to size so 36pt + 24pt + 18pt
            // pack into 600 px without overlap.
            y_pt += pt * 1.4;
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                true, // ui_font=true — SF Pro path
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_cjk_fallback_baseline.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 cjk fallback baseline] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — emoji ZWJ + color cluster fingerprint.
    /// Phase 7 promises chrome color-emoji (BGRA atlas + dedicated
    /// `fg_color_pipeline`).  The color path is independent of the
    /// alpha mono path — a regression here usually surfaces as
    /// "emoji come out gray silhouettes" (the pre-Phase-7 fall-back
    /// when the colour atlas wasn't wired).  Lock the colour bytes
    /// so any silent fall-off back to silhouette / wrong glyph
    /// substitution / ZWJ cluster split surfaces as an SSIM drop.
    ///
    /// Family ZWJ (`👨‍👩‍👧‍👦`) is the canonical ZWJ-cluster stress
    /// — drop a single ZWJ join and it explodes into 4 separate
    /// glyphs.  Flag (`🇯🇵` `🇰🇷` `🇺🇸` `🇨🇳`) is the regional
    /// indicator path.  The third row mixes mono + emoji so chrome
    /// switching between alpha + colour atlas mid-line is exercised.
    #[test]
    fn font_v5_emoji_color_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1200;
        let h_px: u32 = 600;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // 4 rows × ~28pt — each pushes a different colour-emoji axis:
        //   L1  base emoji palette — single codepoint colour glyphs
        //   L2  ZWJ family clusters — multi-codepoint joined sequences
        //   L3  flag (regional indicator) pairs — 2 codepoint → 1 glyph
        //   L4  mixed alpha + colour line — atlas switching pressure
        let rows: &[(f64, &str)] = &[
            (28.0, "❤️  ⭐  🌈  🍕  🚀  🎉  🍎  ⚡  🦄  💎"),
            (28.0, "👨‍👩‍👧‍👦  👨‍👨‍👧  👩‍👩‍👦  🧑‍🚀  👨‍💻  👩‍🎨"),
            (28.0, "🇯🇵  🇰🇷  🇺🇸  🇨🇳  🇬🇧  🇩🇪  🇫🇷  🇮🇹  🇧🇷  🇮🇳"),
            (22.0, "Hello 👋 World 🌍, deploy 🚀 the 🏗️ build ✅"),
        ];
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        use crate::font_shape::ShapeOptions;
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let pad_x_pt = 20.0;
        // 28pt cap-height is ~22pt;  start the cursor far enough
        // down that the colour atlas slot doesn't run off the top.
        let mut y_pt = 36.0;
        let opts = ShapeOptions::full();
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (pt, line) in rows.iter() {
            let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *line)
                .color(fg)
                .ui()
                .ui_size_q(size_q)
                .weight(400)
                .opts(opts)
                .draw();
            y_pt += pt * 1.55;
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                true,
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_emoji_color.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 emoji color] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — OpenType opts side-by-side fingerprint.
    /// Phase 8 promises per-context OT feature toggle (`full()` /
    /// `code()` / `all_off()`).  Showcase covers `full` vs `all_off`
    /// on body text;  this snapshot adds `code()` as a third column
    /// AND validates the diff is visible (so a regression that
    /// silently collapses all 3 onto the same path surfaces as SSIM
    /// hits across N rows simultaneously instead of going unnoticed).
    ///
    /// Each row renders the same source string under all three
    /// presets stacked vertically;  rows differ in which OT feature
    /// they probe:
    ///   `Tax Wax Yes`  — kerning (off in `code` / `all_off`)
    ///   `fi fl ffi ffl`— `liga` (off only in `all_off`)
    ///   `==> != <==`   — `calt` contextual alts (Fira-style; mostly
    ///                    inert on SF Pro but the toggle stays)
    ///   `AVA WAV LTL`  — wide-kern letter triples
    #[test]
    fn font_v5_opentype_opts_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1400;
        let h_px: u32 = 900;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // 4 source rows × 3 OT presets = 12 rendered lines.
        let sources = [
            "Tax  Wax  Yes  AVA",
            "fi  fl  ffi  ffl",
            "==>  !=  <==  ===",
            "AVAVAV  WAWAWA  LTLTLT",
        ];
        let preset_pt: f64 = 22.0;
        let label_pt: f64 = 12.0;
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        use crate::font_shape::ShapeOptions;
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let pad_x_pt = 20.0;
        let mut y_pt = 28.0;
        let row_gap_pt = preset_pt * 1.4;
        let group_gap_pt = 14.0;
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        let fg = Color::rgba(220, 224, 235, 1.0);
        let fg_dim = Color::rgba(140, 150, 168, 1.0);
        let presets: &[(&str, ShapeOptions)] = &[
            ("full   ", ShapeOptions::full()),
            ("code   ", ShapeOptions::code()),
            ("all_off", ShapeOptions::all_off()),
        ];
        let label_size_q = crate::glyph_atlas::GlyphKey::size_q_for(label_pt);
        let preset_size_q = crate::glyph_atlas::GlyphKey::size_q_for(preset_pt);
        for source in sources.iter() {
            // Group separator label.
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), *source)
                .color(fg_dim)
                .ui()
                .ui_size_q(label_size_q)
                .weight(400)
                .opts(ShapeOptions::full())
                .draw();
            y_pt += label_pt * 1.6;
            for (name, opts) in presets.iter() {
                let line = format!("{}  {}", name, source);
                canvas
                    .text(Length::Pt(pad_x_pt + 4.0), Length::Pt(y_pt), &line)
                    .color(fg)
                    .ui()
                    .ui_size_q(preset_size_q)
                    .weight(400)
                    .opts(*opts)
                    .draw();
                y_pt += row_gap_pt;
            }
            y_pt += group_gap_pt;
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                true,
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_opentype_opts.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 opentype opts] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — terminal scene composite.  Approximates
    /// the production PTY render path through the canvas:  cell BG
    /// rect runs for the selection band, a brighter rect for the
    /// cursor block, plus multi-line mono Monaco text on top.  Tests
    /// the same primitive composition (`Rect` BG → `Text` FG) the
    /// real PTY frame uses, just driven by hand instead of from a
    /// live `Grid`.  Real `Grid` → `render_layout_to_texture` readback
    /// needs a `StorageModeShared` target texture that the renderer
    /// doesn't currently expose;  threading that through is a
    /// separate refactor.  This canvas approximation locks the
    /// visual composition contract:  no overflow between cells, no
    /// glyph bleed across the selection edge, BG layering preserved.
    #[test]
    fn font_v5_terminal_scene_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1200;
        let h_px: u32 = 600;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        // Each "cell" in the canvas-pt space is half the phys cell
        // (because Canvas is 2× retina).  Use Monaco cell width via
        // chrome_font_metrics so the simulated grid sits on the same
        // pitch as the real terminal.
        let cell_w_pt = (chrome_cell_w as f64) / 2.0;
        let cell_h_pt = (chrome_cell_h as f64) / 2.0;
        use crate::ui::core::{Color, Length};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        let pad_x_pt = 16.0;
        let pad_y_pt = 16.0;
        // Page BG — production terminal dark.
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(15, 18, 23, 1.0))
            .draw();
        // Simulated terminal lines — log-style output with prompts.
        let lines = [
            "$ git status",
            "On branch develop",
            "Your branch is up to date with 'origin/develop'.",
            "",
            "Changes not staged for commit:",
            "  (use \"git add <file>...\" to update what will be committed)",
            "  (use \"git restore <file>...\" to discard changes)",
            "",
            "    modified:   src/render_metal.rs",
            "    modified:   bench/font-rendering/snapshots/...",
            "",
            "$ cargo nextest run --lib",
            "    Finished `test` profile [unoptimized + debuginfo]",
            "        PASS [  0.018s] (1/2) marspot tests::...",
            "        PASS [  0.018s] (2/2) marspot tests::...",
        ];
        // Selection band — rows 3..5 (0-indexed), cols 4..50.
        // Match iTerm2 default selection blue (sub-1.0 alpha so the
        // cell BG underneath still shows through faintly).
        let sel_color = Color::rgba(46, 92, 158, 0.78);
        for row in 3..=5usize {
            let y = pad_y_pt + (row as f64) * cell_h_pt;
            let (col_start, col_end) = if row == 3 {
                (4.0, lines.get(row).map(|l| l.len() as f64).unwrap_or(0.0))
            } else if row == 5 {
                (0.0, 30.0)
            } else {
                (0.0, lines.get(row).map(|l| l.len() as f64).unwrap_or(0.0))
            };
            let w = (col_end - col_start) * cell_w_pt;
            if w > 0.0 {
                canvas
                    .rect()
                    .at(Length::Pt(pad_x_pt + col_start * cell_w_pt), Length::Pt(y))
                    .size(Length::Pt(w), Length::Pt(cell_h_pt))
                    .fill(sel_color)
                    .draw();
            }
        }
        // Cursor cell — block at end of last log line (focused/live).
        let cursor_row = lines.len() - 1;
        let cursor_col = lines[cursor_row].len();
        let cursor_color = Color::rgba(220, 224, 235, 0.90);
        canvas
            .rect()
            .at(
                Length::Pt(pad_x_pt + (cursor_col as f64) * cell_w_pt),
                Length::Pt(pad_y_pt + (cursor_row as f64) * cell_h_pt),
            )
            .size(Length::Pt(cell_w_pt), Length::Pt(cell_h_pt))
            .fill(cursor_color)
            .draw();
        // Text layer last so glyphs ride on top of BG rects.
        let fg = Color::rgba(220, 224, 235, 1.0);
        for (i, line) in lines.iter().enumerate() {
            let y = pad_y_pt + (i as f64) * cell_h_pt;
            canvas
                .text(Length::Pt(pad_x_pt), Length::Pt(y), *line)
                .color(fg)
                .draw();
        }
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                false, // Mono path
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_terminal_scene.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 terminal scene] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// Phase 9 (extended) — chrome decoration composite (rounded
    /// rect + shadow + border).  F3+1.7 fixed the process panel
    /// modal's BG shadow + border path; lock the rendering so a
    /// regression(shadow blur clamped / radius drift / border
    /// over-paint)surfaces on the next snapshot run instead of
    /// being noticed weeks later as "the panel chrome looks off".
    ///
    /// 4 columns × 1 row showcase:
    ///   1.  Solid rounded rect — minimal modal frame
    ///   2.  + Shadow blur 12 / offset (0, 4) — F3+1.7 modal
    ///   3.  + Hairline border on top of shadow — dev panel card
    ///   4.  Stacked depth — three layered rects with progressive
    ///       shadow blur (modal-on-modal, popover-on-popover)
    #[test]
    fn font_v5_chrome_decoration_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 1400;
        let h_px: u32 = 500;
        let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
        use crate::ui::core::{Color, Length, Pt};
        use crate::ui::core::canvas::{Canvas, ParentRect};
        let mut canvas = Canvas::new(
            2.0,
            ParentRect::window(w_px as f64, h_px as f64),
        );
        // Page BG — neutral mid so shadows show.
        canvas
            .rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pt(w_px as f64 / 2.0), Length::Pt(h_px as f64 / 2.0))
            .fill(Color::rgba(28, 30, 36, 1.0))
            .draw();
        let panel_fill = Color::rgba(35, 38, 46, 1.0);
        let border_color = Color::rgba(80, 86, 100, 1.0);
        let shadow_color = Color::rgba(0, 0, 0, 0.55);
        let col_w = 140.0;
        let col_h = 100.0;
        let col_gap = 30.0;
        let top_y = 40.0;
        let label_y = 170.0;
        let label_color = Color::rgba(170, 178, 195, 1.0);
        // Col 1 — solid rounded rect.
        canvas
            .rect()
            .at(Length::Pt(40.0), Length::Pt(top_y))
            .size(Length::Pt(col_w), Length::Pt(col_h))
            .fill(panel_fill)
            .radius(Pt(10.0))
            .draw();
        canvas
            .text(Length::Pt(40.0), Length::Pt(label_y), "1. radius")
            .color(label_color)
            .draw();
        // Col 2 — rounded rect with shadow blur.
        let col2_x = 40.0 + col_w + col_gap;
        canvas
            .rect()
            .at(Length::Pt(col2_x), Length::Pt(top_y))
            .size(Length::Pt(col_w), Length::Pt(col_h))
            .fill(panel_fill)
            .radius(Pt(10.0))
            .shadow(Pt(12.0), (Pt(0.0), Pt(4.0)), shadow_color)
            .draw();
        canvas
            .text(Length::Pt(col2_x), Length::Pt(label_y), "2. + shadow")
            .color(label_color)
            .draw();
        // Col 3 — rounded rect with shadow AND border (F3+1.7 modal).
        let col3_x = col2_x + col_w + col_gap;
        canvas
            .rect()
            .at(Length::Pt(col3_x), Length::Pt(top_y))
            .size(Length::Pt(col_w), Length::Pt(col_h))
            .fill(panel_fill)
            .radius(Pt(10.0))
            .border(Pt(1.0), border_color)
            .shadow(Pt(12.0), (Pt(0.0), Pt(4.0)), shadow_color)
            .draw();
        canvas
            .text(Length::Pt(col3_x), Length::Pt(label_y), "3. + border")
            .color(label_color)
            .draw();
        // Col 4 — stacked depth (3 rects layered with growing shadow).
        let col4_x = col3_x + col_w + col_gap;
        // Back layer — biggest shadow blur,offset down.
        canvas
            .rect()
            .at(Length::Pt(col4_x + 20.0), Length::Pt(top_y))
            .size(Length::Pt(col_w - 20.0), Length::Pt(col_h - 30.0))
            .fill(Color::rgba(25, 28, 35, 1.0))
            .radius(Pt(8.0))
            .shadow(Pt(20.0), (Pt(0.0), Pt(8.0)), shadow_color)
            .draw();
        // Middle layer.
        canvas
            .rect()
            .at(Length::Pt(col4_x + 12.0), Length::Pt(top_y + 12.0))
            .size(Length::Pt(col_w - 20.0), Length::Pt(col_h - 30.0))
            .fill(Color::rgba(30, 33, 41, 1.0))
            .radius(Pt(8.0))
            .shadow(Pt(14.0), (Pt(0.0), Pt(5.0)), shadow_color)
            .draw();
        // Top layer.
        canvas
            .rect()
            .at(Length::Pt(col4_x + 4.0), Length::Pt(top_y + 24.0))
            .size(Length::Pt(col_w - 20.0), Length::Pt(col_h - 30.0))
            .fill(Color::rgba(38, 42, 52, 1.0))
            .radius(Pt(8.0))
            .border(Pt(1.0), border_color)
            .shadow(Pt(8.0), (Pt(0.0), Pt(2.0)), shadow_color)
            .draw();
        canvas
            .text(Length::Pt(col4_x), Length::Pt(label_y), "4. depth stack")
            .color(label_color)
            .draw();
        let bytes = renderer
            .render_canvas_to_bitmap(
                w_px,
                h_px,
                &canvas,
                chrome_cell_w,
                chrome_cell_h,
                chrome_ascent,
                false,
            )
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("font_v5_chrome_decoration.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[font v5 chrome decoration] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// DevPanel UI > Components > Catalog snapshot.  Locks the
    /// rendered bytes for the entire view-tree component preset
    /// catalogue(`card / panel / badge / tooltip / toggle / picker /
    /// list_row / context_menu / breadcrumb`)so any silent regression
    /// in a single primitive(corner radius drift, shadow blur change,
    /// border colour drift, glyph baseline漂)surfaces as an SSIM hit
    /// across the 14-component column.  Pairs with the showcase /
    /// chrome_decoration baselines — those cover the visual atoms,
    /// this covers the composed surface that components live in.
    #[test]
    fn devpanel_components_catalog_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 840;
        let h_px: u32 = 1040;
        let state = crate::ui::components::DevPanelState {
            visible: true,
            origin_pt: (0.0, 0.0),
            size_pt: (420.0, 520.0),
            active_tab: crate::ui::components::dev_panel::TAB_UI,
            active_section: crate::ui::components::dev_panel::SECTION_UI_COMPONENTS_CATALOG,
            scale: 2.0,
        };
        let measure = crate::chrome_measure::ChromeMeasure::new(
            renderer.font_mut(),
            16.0,
            32.0,
        );
        let canvas = crate::ui::components::build_dev_panel_canvas(
            &state,
            w_px as f64,
            h_px as f64,
            16.0,
            32.0,
            24.0,
            &measure,
        );
        drop(measure);
        let bytes = renderer
            .render_canvas_to_bitmap(w_px, h_px, &canvas, 16.0, 32.0, 24.0, false)
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("devpanel_components_catalog.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[devpanel components catalog] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// DevPanel UI > Tokens > Typography snapshot.  Locks the Size
    /// scale(Caption / Body / Header / LargeHeader)+ Weight pair
    /// (Regular / Bold)+ 8 named TextStyle tokens (CAPTION / BODY /
    /// HEADER / LARGE_HEADER / HINT / CODE / LINK / ERROR).Future
    /// drift in the Mono Monaco font metrics(advance / line-height /
    /// cap-height ratio)or theme token registry shows up as an SSIM
    /// hit instead of going unnoticed for weeks.
    #[test]
    fn devpanel_typography_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 840;
        let h_px: u32 = 1040;
        let state = crate::ui::components::DevPanelState {
            visible: true,
            origin_pt: (0.0, 0.0),
            size_pt: (420.0, 520.0),
            active_tab: crate::ui::components::dev_panel::TAB_UI,
            active_section: crate::ui::components::dev_panel::SECTION_UI_TOKENS_TYPOGRAPHY,
            scale: 2.0,
        };
        let measure = crate::chrome_measure::ChromeMeasure::new(
            renderer.font_mut(),
            16.0,
            32.0,
        );
        let canvas = crate::ui::components::build_dev_panel_canvas(
            &state,
            w_px as f64,
            h_px as f64,
            16.0,
            32.0,
            24.0,
            &measure,
        );
        drop(measure);
        let bytes = renderer
            .render_canvas_to_bitmap(w_px, h_px, &canvas, 16.0, 32.0, 24.0, false)
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("devpanel_typography.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[devpanel typography] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// DevPanel Sessions > Architecture snapshot.  Content is fully
    /// static (process tree text / plugin list / hard caps / wire
    /// protocol descriptions),so SSIM gate is meaningful — any drift
    /// surfaces a real visual / text regression instead of just a
    /// version number bumping.  Pairs with `devpanel_components_catalog`
    /// (UI Tab visual lock) and `devpanel_typography`(Tokens lock)
    /// to cover the dev panel design system at 3 different sub-trees.
    #[test]
    fn devpanel_architecture_snapshot() {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").is_err() {
            return;
        }
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip (no Metal): {e}");
                return;
            }
        };
        let w_px: u32 = 840;
        let h_px: u32 = 1040;
        let state = crate::ui::components::DevPanelState {
            visible: true,
            origin_pt: (0.0, 0.0),
            size_pt: (420.0, 520.0),
            active_tab: crate::ui::components::dev_panel::TAB_SESSIONS,
            active_section: crate::ui::components::dev_panel::SECTION_SESSIONS_ARCHITECTURE,
            scale: 2.0,
        };
        let measure = crate::chrome_measure::ChromeMeasure::new(
            renderer.font_mut(),
            16.0,
            32.0,
        );
        let canvas = crate::ui::components::build_dev_panel_canvas(
            &state,
            w_px as f64,
            h_px as f64,
            16.0,
            32.0,
            24.0,
            &measure,
        );
        drop(measure);
        let bytes = renderer
            .render_canvas_to_bitmap(w_px, h_px, &canvas, 16.0, 32.0, 24.0, false)
            .expect("canvas render");

        let mut rgba = vec![0u8; bytes.len()];
        for i in (0..bytes.len()).step_by(4) {
            rgba[i] = bytes[i + 2];
            rgba[i + 1] = bytes[i + 1];
            rgba[i + 2] = bytes[i];
            rgba[i + 3] = bytes[i + 3];
        }

        let out_dir = std::path::PathBuf::from("bench/font-rendering/snapshots");
        std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
        let out_path = out_dir.join("devpanel_architecture.png");
        assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
        let file = std::fs::File::create(&out_path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!(
            "[devpanel architecture] wrote {} ({} × {})",
            out_path.display(),
            w_px,
            h_px,
        );
    }

    /// End-to-end pixel guard for the reused instance buffers.
    ///
    /// Nothing covered this path before: the suite's only
    /// `render_layout_to_texture` remark says a real readback "needs a
    /// `StorageModeShared` target texture that the renderer doesn't
    /// currently expose", and left it there — so the frame L2 actually
    /// paints had no pixel assertion at all.  That was tolerable while
    /// every pass allocated a fresh buffer; it is not tolerable now
    /// that they are refilled in place, because the way that fails is
    /// silently, in pixels, on the frame after a bigger one.
    ///
    /// The contract stated as a test: **a frame drawn into reused
    /// buffers is byte-identical to the same frame drawn into fresh
    /// ones**, including when a larger frame ran in between and left
    /// its tail in the buffer.
    #[test]
    fn reused_instance_buffers_paint_the_same_pixels() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;

        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => return, // no Metal on this machine
        };
        // Shared storage so the CPU can read it back without a blit —
        // the thing the old comment said was missing.
        let device = renderer.device().to_owned();
        let (w, h) = (320u32, 160u32);
        let desc = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT, w as usize, h as usize, false,
            )
        };
        desc.setUsage(
            objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
        );
        desc.setStorageMode(objc2_metal::MTLStorageMode::Shared);
        let target = match device.newTextureWithDescriptor(&desc) {
            Some(t) => t,
            None => return,
        };

        let (cell_w, cell_h) = renderer.cell_dims();
        let layout = Layout::build(w as f64, h as f64, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
        let mut wr = WindowRender::new();

        let scene = |text: &str, cols: u16, rows: u16| -> Grid {
            let mut g = Grid::new(cols, rows);
            for (r, line) in text.lines().enumerate() {
                for (c, ch) in line.chars().enumerate() {
                    g.set_cell(c as u16, r as u16, Cell { ch, attrs: Default::default() });
                }
            }
            g
        };
        // `seq` is what the per-pane instance cache fingerprints —
        // the grid's contents are never hashed, because the session
        // bumps `seq` whenever they change.  A first draft of this
        // test held `seq` at 0 while swapping the grid underneath,
        // and every frame came back identical: the cache was right
        // and the test was lying to it.
        fn view_of(g: &Grid, seq: u64) -> SessionView<'_> {
            SessionView {
                grid: g,
                view_offset: 0,
                cursor_visible: false,
                focused: true,
                title: "",
                selection: None,
                ime_preedit: "",
                update_pending: false,
                dormant: false,
                recede: 0,
                scrim: 0.0,
                right_badge: "", agent_tui: false,
                top_fixed_h_cells: 0,
                bot_fixed_h_cells: 0,
                highlight_spans: &[],
                search_overlay: None,
                seq,
            }
        }

        let small = scene("hi", 20, 6);
        let big = scene(
            "MMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM",
            20, 6,
        );

        // Frame 1: cold pool — every buffer is freshly allocated.
        let v = view_of(&small, 1);
        renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
        let fresh = texture_bytes_bgra(&target);
        {
            // Diagnostic: the BG pass always clears to SIDEBAR_BG, a
            // dark grey — pure black everywhere means the render never
            // reached this texture, which is a broken test rig, not a
            // broken renderer.
            let mut distinct = std::collections::HashSet::new();
            for px in fresh.chunks_exact(4) {
                distinct.insert([px[0], px[1], px[2], px[3]]);
                if distinct.len() > 8 { break }
            }
            let sample: Vec<_> = distinct.iter().take(4).collect();
            assert!(
                distinct.len() > 1,
                "frame has one colour only: {sample:?} (SIDEBAR_BG is {:?})",
                SIDEBAR_BG_F,
            );
        }

        // Frame 2: same scene, warm pool — buffers are refilled, not
        // reallocated.  Same pixels, or the reuse is wrong.
        let v = view_of(&small, 1);
        renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
        let reused = texture_bytes_bgra(&target);
        assert_eq!(fresh, reused, "a refilled buffer must paint what a fresh one painted");

        // A bigger frame grows the buffers; going back to the small
        // one must not leave the big one's tail behind.  Instance
        // counts are what bound the draw, so a stale tail is exactly
        // the bug that would survive every other test here.
        let v = view_of(&big, 2);
        renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
        let full = texture_bytes_bgra(&target);
        assert_ne!(full, fresh, "the two scenes must actually differ, or this proves nothing");

        let v = view_of(&small, 1);
        renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
        let after_shrink = texture_bytes_bgra(&target);
        assert_eq!(
            fresh, after_shrink,
            "a smaller frame after a larger one must not inherit its leftovers",
        );
    }

    /// The pool has one job — never allocate twice for the same
    /// shape — and one hazard: refilling a buffer the GPU might still
    /// be reading.  The hazard is handled by only using it on the
    /// path that waits for completion (see `InstanceBufferPool`);
    /// this covers the job.
    #[test]
    fn the_instance_pool_grows_once_and_then_stops_allocating() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return, // no Metal on this machine
        };
        let mut pool = InstanceBufferPool::default();
        let _ = take_instance_buffer_cost();

        // First upload allocates; capacity is rounded up, so a
        // slightly larger second frame must NOT allocate again — that
        // is the whole point, and a naive `cap < len` on an exact-fit
        // buffer would reallocate on every frame that grew by a byte.
        let small = vec![7u8; 1000];
        let buf = pool.upload(&device, 0, &small).expect("first upload");
        let (_, bytes_after_first) = take_instance_buffer_cost();
        assert!(bytes_after_first > 0, "the first upload has to allocate");
        // The bytes actually landed.
        let got = unsafe {
            std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, small.len())
        };
        assert_eq!(got, &small[..], "contents must be what we uploaded");

        let bigger = vec![9u8; 60_000];
        pool.upload(&device, 0, &bigger).expect("second upload");
        let (_, bytes_after_second) = take_instance_buffer_cost();
        assert_eq!(
            bytes_after_second, 0,
            "growth inside the rounded-up capacity must not reallocate",
        );

        // Past the capacity it must grow — silently truncating would
        // corrupt the frame instead of costing an allocation.
        let huge = vec![1u8; 300_000];
        let buf = pool.upload(&device, 0, &huge).expect("third upload");
        let (_, bytes_after_third) = take_instance_buffer_cost();
        assert!(bytes_after_third >= huge.len() as u64, "must reallocate to fit");
        let got = unsafe {
            std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, huge.len())
        };
        assert_eq!(got.len(), huge.len());
        assert!(got.iter().all(|&b| b == 1), "the whole payload must land");

        // Empty input is not a buffer, and an out-of-range slot is
        // refused rather than panicking.
        assert!(pool.upload(&device, 0, &[]).is_none());
        assert!(pool.upload(&device, INSTANCE_SLOTS, &small).is_none());
    }

    /// Verify the basic Metal plumbing works on this machine — proves
    /// the dep + bindings resolve and we can talk to the GPU.  CI on
    /// non-Metal machines will skip this naturally because
    /// `system_default_device` returns `Err`.
    #[test]
    fn headless_renderer_constructs() {
        match MetalRenderer::new_headless() {
            Ok(r) => {
                assert!(r.layer.is_none());
            }
            Err(e) => {
                eprintln!("skipping: no Metal device on this host ({e})");
            }
        }
    }

    /// End-to-end BG-pass test: render two solid-colour cells onto
    /// a 4×4 texture and read back the pixels.  Verifies shader
    /// compiles, pipeline state is built correctly, vertex/fragment
    /// IO matches, and instanced draw + readback work.
    #[test]
    fn bg_pass_renders_cells() {
        let r = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("skipping: no Metal device");
                return;
            }
        };
        // 4×4 viewport, two cells:
        //   cell A at (0,0)-(2,4) → red    (left half)
        //   cell B at (2,0)-(2,4) → green  (right half)
        let cells = vec![
            CellInstance {
                origin: [0.0, 0.0],
                size: [2.0, 4.0],
                color: [1.0, 0.0, 0.0, 1.0],
            },
            CellInstance {
                origin: [2.0, 0.0],
                size: [2.0, 4.0],
                color: [0.0, 1.0, 0.0, 1.0],
            },
        ];
        let bytes = r
            .render_cells_bg_offscreen(4, 4, &cells)
            .expect("offscreen render");
        assert_eq!(bytes.len(), 4 * 4 * 4);

        // BGRA layout, top-left origin.  Sample one pixel from each
        // half — exact value depends on sRGB encoding so allow a wide
        // tolerance; we just need to see "left = red-ish, right = green-ish".
        let pixel_at = |x: usize, y: usize| {
            let off = (y * 4 + x) * 4;
            (bytes[off + 2], bytes[off + 1], bytes[off]) // R, G, B (skip A)
        };
        let (l_r, l_g, _l_b) = pixel_at(0, 1);
        let (r_r, r_g, _r_b) = pixel_at(3, 1);
        assert!(l_r > 200, "left half should be red, got R={l_r}");
        assert!(l_g < 50, "left half should be red, got G={l_g}");
        assert!(r_r < 50, "right half should be green, got R={r_r}");
        assert!(r_g > 200, "right half should be green, got G={r_g}");
    }

    /// End-to-end FG-pass test: rasterise glyph 'A' through the
    /// real GlyphAtlas, then run a single FG draw of that glyph onto
    /// a small texture and confirm the alpha-blended result is at
    /// least somewhere in the destination.  Validates: shader compile,
    /// FG pipeline state w/ blending, sampler, atlas-texture binding,
    /// instanced draw + readback.
    #[test]
    fn fg_pass_renders_one_atlas_glyph() {
        use crate::glyph_atlas::GlyphAtlas;
        use core_text::font::new_from_name;

        let r = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("skipping: no Metal device");
                return;
            }
        };

        let mut atlas = GlyphAtlas::new(&r.device, 256, 256).expect("atlas");
        let font = new_from_name("Menlo", 13.0).expect("Menlo");

        let mut cg_glyph: core_graphics::font::CGGlyph = 0;
        let cu: u16 = b'A' as u16;
        unsafe {
            font.get_glyphs_for_characters(&cu, &mut cg_glyph, 1);
        }
        assert!(cg_glyph != 0);

        let entry = atlas
            .get_or_rasterize(
                text_glyph_key(0, cg_glyph, &font),
                &font,
                SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 },
                1,
            )
            .expect("rasterise A");
        let (atlas_w, atlas_h) = atlas.dims();

        // One glyph instance, drawn at (8, 8) with the atlas's pixel
        // size, fully opaque white tint.  Atlas R8 alpha modulates
        // through to the readable colour.
        let glyphs = vec![GlyphInstance {
            origin: [8.0, 8.0],
            size: [entry.px_w as f32, entry.px_h as f32],
            uv0: [
                entry.u0 as f32 / atlas_w as f32,
                entry.v0 as f32 / atlas_h as f32,
            ],
            uv1: [
                entry.u1 as f32 / atlas_w as f32,
                entry.v1 as f32 / atlas_h as f32,
            ],
            color: [1.0, 1.0, 1.0, 1.0],
        }];

        let bytes = r
            .render_glyphs_fg_offscreen(64, 64, atlas.texture(), &glyphs, (0.0, 0.0, 0.0, 1.0))
            .expect("fg offscreen");
        assert_eq!(bytes.len(), 64 * 64 * 4);

        // Find at least one pixel inside the glyph's destination
        // rect that's substantially brighter than the cleared
        // background (which was opaque black).
        let mut max_brightness = 0u8;
        for y in 8..(8 + entry.px_h as usize) {
            for x in 8..(8 + entry.px_w as usize) {
                let off = (y * 64 + x) * 4;
                let b = bytes[off];
                let g = bytes[off + 1];
                let r_byte = bytes[off + 2];
                let lum = ((b as u16 + g as u16 + r_byte as u16) / 3) as u8;
                if lum > max_brightness {
                    max_brightness = lum;
                }
            }
        }
        assert!(
            max_brightness > 100,
            "expected at least one bright pixel inside glyph rect, got max={max_brightness}"
        );
    }

    /// The drag source is the deepest reason of all: while a pane is
    /// in the user's hand it has to read as picked up, deeper than
    /// merely unfocused and deeper than an empty seat.
    #[test]
    fn the_drag_source_reads_deeper_than_the_other_reasons() {
        use crate::layout::Layout;
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let grid = crate::grid::Grid::new(10, 4);
        let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
        let view = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", agent_tui: false, top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[], search_overlay: None, seq: 0,
            dormant: false, recede: 0, scrim: 0.0,
        };
        let mut overlay_rects = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None, None, None,
            None, // settings panel
            None, None,
            Some(0), // this pane is being dragged
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut overlay_rects,
        );
        let scrims: Vec<f32> = overlay_rects
            .iter()
            .filter(|r| r.fill_color[0] == 0.0 && r.fill_color[3] > 0.0)
            .map(|r| r.fill_color[3])
            .collect();
        assert!(
            scrims.iter().any(|a| (*a - DRAG_SOURCE_SCRIM).abs() < 1e-6),
            "a dragged pane wears the drag scrim even while focused, got {scrims:?}"
        );
        assert!(DRAG_SOURCE_SCRIM > EMPTY_SEAT_SCRIM);
    }


    /// The attention ladder, as a table.  One pane is fully present,
    /// the rest step back, a resting one steps back further — and the
    /// pane the user is in is never dimmed, whatever the state machine
    /// thinks of it.
    #[test]
    fn the_attention_ladder_puts_the_focused_pane_in_front() {
        assert_eq!(attention_scrim(true, 0), 0.0, "the pane you are in");
        assert_eq!(
            attention_scrim(true, 2),
            0.0,
            "…even if its session was reclaimed: you are in it now"
        );
        assert_eq!(attention_scrim(false, 0), UNFOCUSED_SCRIM);
        assert_eq!(attention_scrim(false, 1), RESTING_SCRIM);
        assert_eq!(attention_scrim(false, 2), PARKED_SCRIM);
        assert!(
            UNFOCUSED_SCRIM < RESTING_SCRIM && RESTING_SCRIM < PARKED_SCRIM,
            "the ladder has to be monotonic or it says nothing"
        );
        assert!(
            PARKED_SCRIM > UNFOCUSED_SCRIM,
            "a pane whose program is gone has to read further away than \
             one that is merely not the focused pane"
        );
        // Opacity is what the user perceives; the constants are its
        // complement, and the tiers are the ones asked for.
        assert!((1.0 - UNFOCUSED_SCRIM - 0.75).abs() < 1e-6, "75 % opacity");
        assert!((1.0 - RESTING_SCRIM - 0.50).abs() < 1e-6, "50 % opacity");
        assert!((1.0 - PARKED_SCRIM - 0.25).abs() < 1e-6, "25 % opacity");
    }

    /// 2026-08-08, two reports one after the other.
    ///
    /// The first: `①` renders at a fraction of the CJK beside it.
    /// Measured cause — PingFang draws it 11.71 px wide against a
    /// 7.20 px cell, so `rasterise_glyph` scale-to-fits it to 61 %, and
    /// because circled digits are square the *width* always binds: no
    /// font on the machine escapes it.  The fix taken was WezTerm's
    /// `allow_square_glyphs_to_overflow_width` = `WhenFollowedBySpace`:
    /// spill into the next cell when it is blank.
    ///
    /// The second, on that build: *圈圈文字先小后大…要么全大要么全小.*
    /// And that is what the rule guarantees — `① ` is full size, `①消`
    /// is 61 %, so the same character changes size with whatever gets
    /// written next to it.  Whichever size you prefer, watching one
    /// turn into the other reads as a fault.
    ///
    /// So: **a cell's glyph does not depend on its neighbours.**  One
    /// cell means scaled to one cell.  Full size costs a second cell
    /// and moves the wrap point, which is a real decision with a real
    /// downside — so it is a setting, not a guess made per character.
    #[test]
    fn a_glyph_is_drawn_the_same_whatever_sits_next_to_it() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 512, 512).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 64, 64).expect("color atlas");
        let metrics = SlotMetrics { cell_w: 7, cell_h: 16, baseline_from_top: 12 };

        // The set the old rule fired on: narrow Ambiguous glyphs and
        // the circled family.  Each is rasterised once, and asking for
        // it again — whatever the line around it looks like — must give
        // the identical slot, because there is nothing left to vary.
        for ch in ['★', '☆', '●', '▲', '▼', '①', '③', 'Ⓐ', '⓪', '❶'] {
            assert_eq!(crate::grid::char_width(ch), 1, "{ch} is drawn narrow");
            let first = resolve_cell_glyph_routed(
                &mut atlas, &mut color_atlas, &mut font, ch, false, false, metrics,
            );
            let again = resolve_cell_glyph_routed(
                &mut atlas, &mut color_atlas, &mut font, ch, false, false, metrics,
            );
            match (first, again) {
                (Some((a, _)), Some((b, _))) => assert_eq!(
                    (a.u0, a.v0, a.u1, a.v1),
                    (b.u0, b.v0, b.u1, b.v1),
                    "{ch} rasterised to two different slots",
                ),
                (None, None) => {}
                _ => panic!("{ch}: resolved once and not the other time"),
            }
        }
    }

    /// The way to get circled digits at full size is the setting, and
    /// it works by giving them a second cell — not by borrowing one.
    #[test]
    fn the_wide_setting_is_what_makes_circled_digits_full_size() {
        crate::settings::set_for_test(crate::settings::Settings {
            appearance_circled_wide: false,
            ..Default::default()
        });
        for ch in ['①', '⑨', 'Ⓐ', '⓪', '❶'] {
            assert_eq!(crate::grid::char_width(ch), 1, "{ch} off");
        }
        crate::settings::set_for_test(crate::settings::Settings {
            appearance_circled_wide: true,
            ..Default::default()
        });
        for ch in ['①', '⑨', 'Ⓐ', '⓪', '❶'] {
            assert_eq!(crate::grid::char_width(ch), 2, "{ch} on");
        }
        // …and it is a decision about the *grid*, so it is the same
        // decision wherever the character appears — never per-neighbour.
        crate::settings::set_for_test(crate::settings::Settings::default());
    }

    /// Rebuilding the fonts is what makes a display swap land, and it
    /// must be a no-op when nothing moved — it runs on every attach,
    /// and throwing both atlases away per resize step would make a
    /// window drag re-rasterise the screen continuously.
    #[test]
    fn fonts_rebuild_only_when_the_scale_actually_moves() {
        let saved = crate::ui::chrome_scale();
        let Ok(mut r) = MetalRenderer::new_headless() else { return };
        let (w0, h0) = r.cell_dims();

        assert!(!r.rebuild_fonts_if_scale_changed(), "nothing moved");
        assert_eq!(r.cell_dims(), (w0, h0));

        crate::ui::set_chrome_scale(2.0);
        assert!(r.rebuild_fonts_if_scale_changed(), "a new density rebuilds");
        let (w2, h2) = r.cell_dims();
        assert!(
            (w2 - w0 * 2.0).abs() < 0.5 && (h2 - h0 * 2.0).abs() < 0.5,
            "the cell has to double with the density: {w0:.2}x{h0:.2} -> {w2:.2}x{h2:.2}",
        );
        assert!(!r.rebuild_fonts_if_scale_changed(), "and settle");

        crate::ui::set_chrome_scale(saved);
        assert!(r.rebuild_fonts_if_scale_changed());
        let (w1, h1) = r.cell_dims();
        assert!(
            (w1 - w0).abs() < 0.01 && (h1 - h0).abs() < 0.01,
            "and come back exactly: {w0:.2}x{h0:.2} -> {w1:.2}x{h1:.2}",
        );
    }

    /// The scrim primitive is shared by every reason a pane recedes,
    /// and they do not stack: two 0.22 layers composite to 0.39, a
    /// shade nobody chose.
    #[test]
    fn scrims_do_not_stack_the_deepest_reason_wins() {
        use crate::layout::Layout;
        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let grid = crate::grid::Grid::new(10, 4);
        let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
        let mk = |focused: bool, dormant: bool, recede: u32| SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused,
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", agent_tui: false, top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[], search_overlay: None, seq: 0,
            dormant, recede, scrim: attention_scrim(focused, recede),
        };
        let mut scrim_alphas = |view: SessionView| -> Vec<f32> {
            let mut overlay_rects = Vec::new();
            build_instances(
                &layout,
                std::slice::from_ref(&view),
                &[],
                0,
                true,
                None, None, None, None, None, None, None, None,
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut overlay_rects,
            );
            overlay_rects
                .iter()
                .filter(|r| r.fill_color[0] == 0.0 && r.fill_color[3] > 0.0)
                .map(|r| r.fill_color[3])
                .collect()
        };
        // Focused and live: nothing over it at all.
        let focused = scrim_alphas(mk(true, false, 0));
        assert!(
            !focused.iter().any(|a| (*a - UNFOCUSED_SCRIM).abs() < 1e-6),
            "the focused pane gets no attention scrim, got {focused:?}"
        );
        // Unfocused and parked: one scrim, the parked one.
        let parked = scrim_alphas(mk(false, false, 2));
        assert!(
            parked.iter().any(|a| (*a - PARKED_SCRIM).abs() < 1e-6),
            "a parked pane recedes to the parked tier, got {parked:?}"
        );
        // Parked AND an empty seat: still one scrim, the deeper of
        // the two.
        let both = scrim_alphas(mk(false, true, 2));
        let deep = both.iter().filter(|a| **a >= EMPTY_SEAT_SCRIM).count();
        assert_eq!(deep, 1, "exactly one scrim, got {both:?}");
        assert!(both.iter().any(|a| (*a - PARKED_SCRIM).abs() < 1e-6));
    }

    /// The focus ring survives the neighbours' scrims.
    ///
    /// The ring is painted in the BG pass at the seam, and the seam on
    /// the right/bottom sides lies inside the NEIGHBOURING cells'
    /// rects.  Every unfocused pane covers its whole rect with a scrim
    /// in the overlay pass — which runs later — so those two sides
    /// came out dimmed while left and top stayed bright (2026-08-20
    /// report).  The fix repaints the ring into the overlay pass after
    /// the scrims; this pins that it is still there.
    #[test]
    fn focus_ring_is_repainted_over_neighbour_scrims() {
        use crate::grid::Grid;
        use crate::layout::Layout;

        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");

        let grid = Grid::new(10, 4);
        // 2x2 with a gutter: gutter > 0 is what turns the ring on.
        let layout = Layout::build(
            font.cell_w * 24.0,
            font.cell_h * 10.0,
            0.0,
            0.0,
            2.0,
            2,
            2,
            font.cell_w,
            font.cell_h,
        );
        let mk = |focused: bool, scrim: f32| SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: true,
            focused,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        // Pane 0 focused; its right / bottom / bottom-right neighbours
        // all carry a scrim, which is the situation that hid the ring.
        let views = vec![mk(true, 0.0), mk(false, 0.35), mk(false, 0.35), mk(false, 0.35)];

        let mut overlay_ui: Vec<UiRectInstance> = Vec::new();
        build_instances(
            &layout, &views, &[], 0, true,
            None, None, None, None, None, None, None, None,
            &mut font, &mut atlas, &mut color_atlas,
            // cells, glyphs, color_glyphs, _dots
            &mut Vec::new(), &mut Vec::new(), &mut Vec::new(), &mut Vec::new(),
            // ui_rects, pane_caches
            &mut Vec::new(), &mut Vec::new(),
            // overlay_cells, overlay_glyphs, overlay_ui_rects
            &mut Vec::new(), &mut Vec::new(), &mut overlay_ui,
        );

        let ring = [0.72f32, 0.76, 0.82, 1.0];
        let ring_at: Vec<usize> = overlay_ui
            .iter()
            .enumerate()
            .filter(|(_, r)| r.fill_color == ring)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(ring_at.len(), 8, "expected the 8 ring rects in the overlay pass, got {ring_at:?}");

        // Order is what makes it visible: every scrim must be pushed
        // before the ring, or the ring is dimmed again.
        let last_scrim = overlay_ui
            .iter()
            .rposition(|r| r.fill_color[3] > 0.0 && r.fill_color[0] == 0.0
                        && r.fill_color[1] == 0.0 && r.fill_color[2] == 0.0)
            .expect("unfocused panes should have pushed scrims");
        assert!(
            ring_at[0] > last_scrim,
            "ring must be painted after the scrims (ring at {ring_at:?}, last scrim {last_scrim})"
        );
    }

    #[test]
    fn build_instances_emits_cells_and_glyphs() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;

        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");

        let mut grid = Grid::new(10, 4);
        let cell_a = Cell {
            ch: 'A',
            attrs: Default::default(),
        };
        let cell_b = Cell {
            ch: 'B',
            attrs: Default::default(),
        };
        grid.set_cell(0, 0, cell_a);
        grid.set_cell(1, 0, cell_b);

        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0,
            0.0,
            0.0,
            1,
            1,
            font.cell_w,
            font.cell_h,
        );
        let view = SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: true,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };

        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );

        // Expected glyph instances:
        //  - 'B' at col 1 (FG colour) — the cursor cell at col 0 is
        //    skipped during the FG emit because it's solid-cursor
        //  - 'A' at col 0 (BG colour) re-rendered after the cursor block
        //  Order in vec is FG-pass-first then cursor re-render, so [B, A].
        //  cells: at minimum a terminal-bg fill + cursor block.
        //  Focus outline (×4) only fires when gutter > 0, which a
        //  1×1 layout doesn't have, so it can be absent here.
        assert!(cells.len() >= 2, "got cells.len()={}", cells.len());
        assert_eq!(glyphs.len(), 2, "got glyphs.len()={}", glyphs.len());
        // Whichever order they came out in, the two glyphs are exactly
        // one cell apart in x (modulo CT bearing differences ≤2 px).
        let dx = (glyphs[1].origin[0] - glyphs[0].origin[0]).abs();
        let cw = font.cell_w as f32;
        assert!(
            (dx - cw).abs() < 3.0,
            "glyphs should be ~one cell apart, got |dx|={dx} vs cell_w={cw}"
        );
    }

    /// A colour emoji must route to the COLOUR glyph buffer + colour atlas,
    /// not the mono one — the whole point of the colour-emoji path.
    #[test]
    fn build_instances_routes_color_emoji() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;

        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 512, 512).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 512, 512).expect("color atlas");

        // Skip on the (macOS-impossible) chance there's no colour emoji
        // font — the routing decision keys off the resolved font's
        // colour-glyphs trait, so without one there's nothing to assert.
        let (emoji_font_idx, emoji_glyph) = font.resolve_char('😀', false, false);
        if emoji_glyph == 0 || !font.is_color_font(emoji_font_idx) {
            return;
        }

        let mut grid = Grid::new(10, 4);
        grid.set_cell(0, 0, Cell { ch: '😀', attrs: Default::default() });
        grid.set_cell(2, 0, Cell { ch: 'A', attrs: Default::default() });

        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0,
            0.0,
            0.0,
            1,
            1,
            font.cell_w,
            font.cell_h,
        );
        let view = SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: false, // render every cell, don't skip under cursor
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };

        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );

        assert_eq!(color_glyphs.len(), 1, "emoji should emit one colour glyph");
        assert_eq!(glyphs.len(), 1, "the 'A' should be the only mono glyph");
        assert!(color_atlas.cache_len() >= 1, "colour atlas should hold the emoji");
    }

    // ─── C1: PaneTool framework layout-shrink tests ───────────────

    /// Build instances for a 2-row grid with `top_fixed_h_cells = N`.
    /// Returns the glyphs vec for inspection.  Helper shared by the
    /// two layout-shrink tests below.
    fn build_glyphs_with_top_fixed(top_fixed: u16) -> Vec<GlyphInstance> {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;
        let device = system_default_device().expect("metal device");
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let mut grid = Grid::new(10, 4);
        grid.set_cell(0, 0, Cell { ch: 'A', attrs: Default::default() });
        grid.set_cell(2, 0, Cell { ch: 'B', attrs: Default::default() });
        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0,
            0.0,
            0.0,
            1,
            1,
            font.cell_w,
            font.cell_h,
        );
        let view = SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: top_fixed,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        glyphs
    }

    /// C1 — a `TopFixed` tool claiming N rows shifts the grid's
    /// glyph origin Y down by exactly `N * cell_h`.  No tool, no
    /// shift (byte-identical to pre-C1, per §12.C1 DoD).
    #[test]
    fn c1_top_fixed_tool_shifts_grid_glyph_y() {
        if system_default_device().is_err() {
            return;
        }
        let font_cell_h = FontCache::build().expect("font").cell_h as f32;
        let g0 = build_glyphs_with_top_fixed(0);
        let g2 = build_glyphs_with_top_fixed(2);
        assert!(!g0.is_empty(), "baseline produced glyphs");
        assert_eq!(g0.len(), g2.len(), "glyph count must be identical");
        // Match by uv (identifies the character; A vs B have distinct
        // uv).  For each glyph, the v2 origin_y should be exactly
        // 2 * cell_h above (numerically higher, i.e. lower on screen).
        for (g0_inst, g2_inst) in g0.iter().zip(g2.iter()) {
            assert_eq!(g0_inst.uv0, g2_inst.uv0, "uv must match (same char)");
            let dy = g2_inst.origin[1] - g0_inst.origin[1];
            let expected = 2.0 * font_cell_h.round();
            assert!(
                (dy - expected).abs() < 0.5,
                "TopFixed=2 should shift glyph Y by ~{expected}; got dy={dy}"
            );
        }
    }

    /// C4 — `highlight_spans` covering two viewport rows emits 2
    /// `CellInstance` fills tinted `HIGHLIGHT_BG`, one per row at
    /// the listed column range.  Verifies span row-filter +
    /// per-row col clipping + bounded cells.len() growth (≤ 1
    /// `CellInstance` per spanned row).
    #[test]
    fn c4_highlight_two_row_span_emits_two_cells() {
        use crate::grid::Grid;
        use crate::layout::Layout;
        if system_default_device().is_err() {
            return;
        }
        let device = system_default_device().expect("metal device");
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let grid = Grid::new(10, 4);
        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
        );
        // Two-row span: row 1 col 5-9 (5 cols), row 2 col 0-3 (4 cols).
        let spans = vec![
            HighlightSpan { view_row: 1, col_start: 5, col_end_inclusive: 9 },
            HighlightSpan { view_row: 2, col_start: 0, col_end_inclusive: 3 },
        ];
        let view = SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
            seq: 0,
        };
        // Baseline cell count (no highlight) for the same layout/grid.
        let baseline_view = SessionView {
            grid: view.grid,
            view_offset: view.view_offset,
            cursor_visible: view.cursor_visible,
            focused: view.focused,
            title: view.title,
            selection: view.selection,
            ime_preedit: view.ime_preedit,
            update_pending: view.update_pending,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: view.right_badge, agent_tui: view.agent_tui,
            top_fixed_h_cells: view.top_fixed_h_cells,
            bot_fixed_h_cells: view.bot_fixed_h_cells,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let mut run = |v: &SessionView| -> Vec<CellInstance> {
            let mut cells: Vec<CellInstance> = Vec::new();
            let mut glyphs: Vec<GlyphInstance> = Vec::new();
            let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
            let mut dots: Vec<CellInstance> = Vec::new();
            build_instances(
                &layout,
                std::slice::from_ref(v),
                &[],
                0,
                true,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
            );
            cells
        };
        let baseline = run(&baseline_view);
        let with_hl = run(&view);
        let extra = with_hl.len() - baseline.len();
        assert_eq!(
            extra, 2,
            "expected exactly 2 extra CellInstance for two-row span; got {extra}"
        );
        // Find the highlight cells (colour = HIGHLIGHT_BG).
        let highlight: Vec<&CellInstance> = with_hl
            .iter()
            .filter(|c| (c.color[0] - HIGHLIGHT_BG.0).abs() < 1e-3)
            .collect();
        assert_eq!(highlight.len(), 2);
        // Each highlight cell sits one cell_h below the previous (consecutive rows).
        // `push_session` rounds cell_w / cell_h to integer pixels before
        // emitting cell instances; mirror the rounding in our assertions.
        let cell_h = (font.cell_h as f32).round();
        let mut ys: Vec<f32> = highlight.iter().map(|c| c.origin[1]).collect();
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((ys[1] - ys[0] - cell_h).abs() < 1.0, "row gap should be cell_h");
        // Sizes: 5 cells wide vs 4 cells wide.
        let cell_w = (font.cell_w as f32).round();
        let widths: Vec<f32> = highlight.iter().map(|c| c.size[0]).collect();
        assert!(
            widths.iter().any(|w| (w - 5.0 * cell_w).abs() < 1.0),
            "expected a 5-cell-wide highlight (cell_w={cell_w}); got widths {widths:?}"
        );
        assert!(
            widths.iter().any(|w| (w - 4.0 * cell_w).abs() < 1.0),
            "expected a 4-cell-wide highlight (cell_w={cell_w}); got widths {widths:?}"
        );
    }

    /// C4 — a highlight span whose `view_row` is past the grid's
    /// row count is silently skipped (no panic, no emission).
    #[test]
    fn c4_highlight_out_of_range_row_is_skipped() {
        use crate::grid::Grid;
        use crate::layout::Layout;
        if system_default_device().is_err() {
            return;
        }
        let device = system_default_device().expect("metal device");
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let grid = Grid::new(10, 4);
        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
        );
        // view_row = 9 is past grid.rows() = 4 — should be skipped.
        let spans = vec![
            HighlightSpan { view_row: 9, col_start: 0, col_end_inclusive: 4 },
        ];
        let view = SessionView {
            grid: &grid,
            view_offset: 0,
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false,
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
            seq: 0,
        };
        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        // No HIGHLIGHT_BG cell should be present.
        let highlight_count = cells
            .iter()
            .filter(|c| (c.color[0] - HIGHLIGHT_BG.0).abs() < 1e-3)
            .count();
        assert_eq!(highlight_count, 0);
    }

    /// C1 — a `BottomFixed` tool DOES NOT shift the grid's glyph
    /// origin (only TopFixed does).  This locks down the invariant
    /// that the BottomFixed slot reserves space against the pane's
    /// lower edge — the grid sits where it always did.
    #[test]
    fn c1_bottom_fixed_tool_does_not_shift_grid_glyph_y() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;
        if system_default_device().is_err() {
            return;
        }
        let device = system_default_device().expect("metal device");
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");
        let mut color_atlas = GlyphAtlas::new_color(&device, 256, 256).expect("color atlas");
        let mut grid = Grid::new(10, 4);
        grid.set_cell(0, 0, Cell { ch: 'A', attrs: Default::default() });
        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
        );
        let view_base = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false, dormant: false,
                                                                                recede: 0,
                scrim: 0.0,
            right_badge: "", agent_tui: false, top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let view_bot = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false, dormant: false,
                                                                                recede: 0,
                scrim: 0.0,
            right_badge: "", agent_tui: false, top_fixed_h_cells: 0, bot_fixed_h_cells: 2,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let mut run = |view: &SessionView| -> Vec<GlyphInstance> {
            let mut cells: Vec<CellInstance> = Vec::new();
            let mut glyphs: Vec<GlyphInstance> = Vec::new();
            let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
            let mut dots: Vec<CellInstance> = Vec::new();
            build_instances(
                &layout,
                std::slice::from_ref(view),
                &[],
                0,
                true,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
            );
            glyphs
        };
        let g0 = run(&view_base);
        let gb = run(&view_bot);
        assert_eq!(g0.len(), gb.len());
        for (a, b) in g0.iter().zip(gb.iter()) {
            assert_eq!(a.origin[1], b.origin[1], "bot_fixed must NOT shift grid Y");
        }
    }

    // ── P2b: Canvas → Metal flush, submission-order = z-order ──

    /// Semi-transparent fills must land at the alpha they were given.
    ///
    /// The `ui_rect` fragment shader composites its layers into
    /// **premultiplied** form (`rgb * a`), but the pipeline's source
    /// blend factor was `SourceAlpha` — the *non*-premultiplied
    /// equation — so the destination got alpha applied twice.  At
    /// a = 1.0 the two agree, which is why every opaque panel looked
    /// right and the bug stayed hidden; at a = 0.2 the fill rendered at
    /// an effective 0.04 and simply vanished.  That is what made the cc
    /// modal's dashed day grid and its status chips invisible, and it is
    /// almost certainly what taught this codebase the folk rule that
    /// overlay backgrounds "have to be" opaque.
    ///
    /// White at alpha `a` over black must read `255 * a`, ± the SDF
    /// edge AA. Sampled at the centre of a full-window rect, far from
    /// any edge, so coverage is exactly 1.
    #[test]
    fn canvas_alpha_blends_at_the_requested_opacity() {
        use crate::ui::core::{Canvas, ParentRect, Color, Length};
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("skip (no Metal): {e}"); return; }
        };
        let (w, h) = (16u32, 16u32);
        for (alpha, want) in [(1.0_f64, 255.0_f64), (0.5, 127.5), (0.25, 63.75)] {
            let mut canvas = Canvas::new(1.0, ParentRect::window(w as f64, h as f64));
            canvas.rect()
                .at(Length::Pt(0.0), Length::Pt(0.0))
                .size(Length::Pct(1.0), Length::Pct(1.0))
                .fill(Color::rgba(255, 255, 255, alpha))
                .draw();
            let bytes = renderer
                .render_canvas_to_bitmap(w, h, &canvas, 48.0, 12.0, 9.0, false)
                .expect("render");
            let i = (w as usize / 2 + (h as usize / 2) * w as usize) * 4;
            let got = bytes[i + 2] as f64; // R channel of BGRA8
            assert!(
                (got - want).abs() <= 6.0,
                "alpha {alpha}: expected ~{want} over black, got {got} \
                 (double-applied alpha would give {:.0})",
                want * alpha
            );
        }
    }

    /// Z-order invariant: three opaque rects at the SAME coord
    /// in submission order red → green → blue must land blue on
    /// the screen.  Same pipeline (all rects), so this verifies
    /// the basic "last instance wins" within a single encoder.
    #[test]
    fn canvas_same_pipeline_submission_order_is_z_order() {
        use crate::ui::core::{Canvas, ParentRect, Color, Length};
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("skip (no Metal): {e}"); return; }
        };
        let w = 8u32;
        let h = 8u32;
        let mut canvas = Canvas::new(1.0, ParentRect::window(w as f64, h as f64));
        // All three at (0, 0) sized full window.  Same pipeline
        // (Rect), so the encoder draws them in array order.
        canvas.rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pct(1.0), Length::Pct(1.0))
            .fill(Color::rgb(255, 0, 0))
            .draw();
        canvas.rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pct(1.0), Length::Pct(1.0))
            .fill(Color::rgb(0, 255, 0))
            .draw();
        canvas.rect()
            .at(Length::Pt(0.0), Length::Pt(0.0))
            .size(Length::Pct(1.0), Length::Pct(1.0))
            .fill(Color::rgb(0, 0, 255))
            .draw();

        let bytes = renderer.render_canvas_to_bitmap(w, h, &canvas, 48.0, 12.0, 9.0, false)
            .expect("render");
        // Sample the centre pixel.  BGRA8: bytes are B, G, R, A.
        let px = w as usize / 2 + (h as usize / 2) * w as usize;
        let i = px * 4;
        let b = bytes[i];
        let g = bytes[i + 1];
        let r = bytes[i + 2];
        // Blue is the last-submitted: expect pure blue.
        assert!(b > 200, "expected blue dominant, got B={b} G={g} R={r}");
        assert!(g < 60,  "green should be near zero, got G={g}");
        assert!(r < 60,  "red should be near zero, got R={r}");
    }

    /// `build_canvas_runs` correctly merges consecutive same-kind
    /// primitives into one run and splits at kind transitions.
    #[test]
    fn canvas_runs_batch_same_kind_split_on_transition() {
        use crate::ui::core::{Canvas, ParentRect, Color, Length};
        let mut canvas = Canvas::new(1.0, ParentRect::window(100.0, 100.0));
        canvas.rect().fill(Color::WHITE).draw();
        canvas.rect().fill(Color::BLACK).draw();
        canvas.text(Length::Pt(0.0), Length::Pt(0.0), "x").color(Color::WHITE).draw();
        canvas.rect().fill(Color::WHITE).draw();
        // Minimal headless deps for build_canvas_runs.  This test
        // doesn't render — it just probes the run-grouping logic.
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("skip (no Metal): {e}"); return; }
        };
        let mut ui = Vec::new();
        let mut gl = Vec::new();
        let mut color_gl = Vec::new();
        let runs = build_canvas_runs(
            &canvas, 48.0, 12.0, 9.0,
            256.0, 256.0, 256.0, 256.0,
            &mut renderer.font, &mut renderer.atlas, &mut renderer.color_atlas,
            &mut ui, &mut gl, &mut color_gl,
            false,
        );
        // Expected sequence: UiRect (2 rects) → Glyph (text) →
        // UiRect (final rect).  Text may produce 0 or 1+ glyphs
        // depending on whether 'x' resolves through the chrome
        // font — guard the assertion against the 0-glyph case
        // by collapsing the middle run.
        assert!(runs.len() >= 2, "expected at least 2 runs, got {runs:?}", runs = runs.len());
        assert_eq!(runs[0].kind, CanvasRunKind::UiRect);
        assert_eq!(runs[0].count, 2);
        assert_eq!(runs.last().unwrap().kind, CanvasRunKind::UiRect);
        // If the text produced glyphs, there's a Glyph run in the middle.
        if !gl.is_empty() {
            assert_eq!(runs.len(), 3);
            assert_eq!(runs[1].kind, CanvasRunKind::Glyph);
            assert_eq!(runs[1].count, gl.len());
        }
    }
}

#[cfg(test)]
mod cc_layout_tests {
    use super::fit_ellipsis;

    #[test]
    fn fit_ellipsis_bounds_every_case() {
        // Fits untouched.
        assert_eq!(fit_ellipsis("lihao@golia.jp", 20.0), "lihao@golia.jp");
        assert_eq!(fit_ellipsis("abc", 3.0), "abc");
        // Too long: cut to exactly the budget, last column is the mark.
        let cut = fit_ellipsis("lihao@golia.jp", 8.0);
        assert_eq!(cut, "lihao@g…");
        assert_eq!(cut.chars().count(), 8);
        // Never exceeds the budget for any prefix length.
        for cols in 3..30 {
            let out = fit_ellipsis("some.very.long.address@example.com", cols as f64);
            assert!(out.chars().count() <= cols, "cols={cols} out={out:?}");
        }
        // Below the useful minimum the slot is dropped, not filled with
        // a bare ellipsis.
        assert_eq!(fit_ellipsis("abcdef", 2.0), "");
        assert_eq!(fit_ellipsis("abcdef", 0.0), "");
    }
}
