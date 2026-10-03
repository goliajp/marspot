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
//!    LRU-evict to keep growth bounded.
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
//! and `Linear` only for the glyph pass.

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
    MTLSamplerState, MTLScissorRect, MTLStoreAction, MTLTexture,
};
use core_graphics::color_space::{kCGColorSpaceSRGB, CGColorSpace};
use foreign_types::ForeignType;
use objc2_app_kit::NSColor;
use objc2_quartz_core::{kCAGravityTopLeft, CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use objc2_metal::MTLBuffer;

use crate::font_cache::FontCache;
use crate::glyph_atlas::{GlyphAtlas, SlotMetrics};
use crate::layout::Layout;
use crate::ui::components::process_panel::ProcessPanelRender;
use crate::ui::components::cc_usage_modal::CcUsageRender;
use crate::ui::components::settings_paint::SettingsRender;
use crate::ui::components::layout_modal_paint::LayoutModalRender;
use crate::ui::components::context_menu_paint::ContextMenuRender;
use crate::render::{SessionView, SidebarEntry};
#[cfg(test)]
use crate::render::HighlightSpan;

use crate::frame_build::palette::*;
use crate::frame_build::frame::build_instances;
use crate::frame_build::canvas_runs::{build_canvas_runs, CanvasRunKind};
use crate::frame_build::pane_cache::PaneInstanceCache;
use crate::frame_build::glyph_resolve::resolve_cell_glyph;

/// One flat rectangle the GPU reads, which is the published one.
///
/// Cell backgrounds, underlines, cursors, rules and the small round
/// indicators are all this shape, and so is `Kind::Rect` and
/// `Kind::Circle` in the format -- which is why one struct serves two
/// pipelines here exactly as it does there.
pub use golia_ui_core::scene::RectInstance as CellInstance;


/// One glyph the GPU reads, which is the published one.
///
/// `uv0` / `uv1` are normalised 0..1 atlas coordinates -- the glyph's
/// slot, top-left and bottom-right. Both atlases, mono and colour, are
/// drawn from this shape, which is why `Kind::Glyph` and
/// `Kind::ColorGlyph` share it in the format too.
pub use golia_ui_core::scene::GlyphInstance;


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
    /// One frame in flight, never two — per window, which is the part
    /// that needed saying: the two surfaces still alternate with a full
    /// frame between reuses, and the instance pool may still be
    /// refilled in place, but only because the pool below is this
    /// window's own.  While the frame was awaited, one pool for all
    /// windows was sound; it stopped being sound here.
    in_flight: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// This window's per-pass instance buffers, refilled frame to
    /// frame on the IOSurface path.  See [`InstanceBufferPool`].
    ///
    /// Per window, because the frame that reads them outlives the call
    /// that fills them and `in_flight` is per window too: one pool
    /// shared by all windows meant window B's fill overwrote the
    /// instances window A's in-flight frame was still reading, and
    /// window A drew window B's content — at window B's coordinates,
    /// so in window A's top-left corner, for one frame (2026-09-27
    /// report: "window2 会被显示到 window1 左上角，又消失").
    instance_pool: InstanceBufferPool,
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
            instance_pool: InstanceBufferPool::default(),
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
    /// Call from anywhere the visible BG region shape is about to
    /// change (layout mode switch, sidebar toggle, resize, surface
    /// reattach).  See the `clear_bg_required` field doc for the race
    /// that motivates the Load-default for steady-state frames.
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

/// F1+11 — one UI rect's draw data, layout-compatible with `UiRect`
/// in `src/shaders/cells.metal`.  A real pixel-mode rounded rectangle
/// with anti-aliased corners, optional border stroke, and optional
/// soft drop shadow — all computed in one fragment shader.
///
/// `origin` + `size` describe the FILL rect in physical pixels (NOT
/// inflated by shadow).  `corner_radius` is also in pixels.  Set
/// `shadow_blur = 0` to skip the shadow path entirely (the SDF still
/// runs but contributes nothing).
/// The rounded-rect instance the GPU reads, which is the published
/// one.
///
/// This used to be a struct declared here, laid out to match a Metal
/// struct in the shader -- two declarations of one layout, agreeing by
/// inspection. The format has an encoder of its own now and the bytes
/// come from it, so there is one declaration and the shader reads what
/// it writes.
pub use golia_ui_core::scene::UiRectInstance;

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
    /// The same rects, read out of a `Scene` slab.  Built alongside so
    /// the two paths can be held against each other on real pixels.
    scene_ui_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
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
    /// The two atlases' textures, bound by the glyph passes.
    atlas_texture: std::sync::Arc<MetalAtlasTexture>,
    color_atlas_texture: std::sync::Arc<MetalAtlasTexture>,
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
    /// Bytes for the rounded-rect passes, written by the published
    /// encoder and reused frame to frame.
    ui_slab: Vec<u8>,
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
    /// open, `None` when closed.  See `layout_modal_paint::LayoutModalRender`.
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
        let scene_ui_pipeline = build_scene_ui_pipeline(&device, &library)?;
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
        let (atlas, atlas_texture) = new_atlas(&device, 4096, 4096, false, font.font_table())?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let (color_atlas, color_atlas_texture) = new_atlas(&device, 1024, 1024, true, font.font_table())?;

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
            scene_ui_pipeline,
            font,
            atlas,
            color_atlas,
            atlas_texture,
            color_atlas_texture,
            cells_scratch: Vec::new(),
            dots_scratch: Vec::new(),
            ui_rects_scratch: Vec::new(),
            ui_slab: Vec::new(),
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
        let scene_ui_pipeline = build_scene_ui_pipeline(&device, &library)?;
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
        let (atlas, atlas_texture) = new_atlas(&device, 4096, 4096, false, font.font_table())?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let (color_atlas, color_atlas_texture) = new_atlas(&device, 1024, 1024, true, font.font_table())?;
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
            scene_ui_pipeline,
            font,
            atlas,
            color_atlas,
            atlas_texture,
            color_atlas_texture,
            cells_scratch: Vec::new(),
            dots_scratch: Vec::new(),
            ui_rects_scratch: Vec::new(),
            ui_slab: Vec::new(),
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
        let drawable = match layer.nextDrawable() {
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
        let drawable = match layer.nextDrawable() {
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
            &self.scene_ui_pipeline,
            &self.fg_pipeline,
            &self.fg_color_pipeline,
            &self.fg_sampler,
            &mut self.atlas,
            &mut self.color_atlas,
            self.atlas_texture.texture(),
            self.color_atlas_texture.texture(),
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
            ref scene_ui_pipeline,
            ref mut font,
            ref mut atlas,
            ref mut color_atlas,
            ref atlas_texture,
            ref color_atlas_texture,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            ref mut color_glyphs_scratch,
            ref mut dots_scratch,
            ref mut ui_rects_scratch,
            ref mut ui_slab,
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
        let drawable = match layer.nextDrawable() {
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
            scene_ui_pipeline,
            fg_sampler,
            atlas_texture.texture(),
            color_atlas_texture.texture(),
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
            ui_slab,
        );

        // Dev panel — Canvas-based overlay. Encoded BEFORE the
        // context menu so the menu (if open) sits on top.
        let chrome_cell_w = font.cell_w as f32;
        let chrome_cell_h = font.cell_h as f32;
        let chrome_ascent = font.ascent as f32;
        let viewport_px = [width_px as f32, height_px as f32];
        if let Some(dev_state) = self.dev_panel_state.as_ref()
            && dev_state.visible {
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
                encode_canvas_into(
                    &canvas, &texture, &cmd,
                    scene_ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                    atlas, color_atlas, atlas_texture.texture(), color_atlas_texture.texture(), device, font,
                    None, &viewport_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    false,
                );
            }

        // P2c — ContextMenu draws OUT-OF-BAND via the Canvas
        // pipeline: built fresh per frame and encoded with
        // submission-order = z-order semantics, AFTER the main
        // encode_passes so it sits above every other overlay.
        if let Some(menu_state) = self.context_menu_state.as_ref() {
            let canvas = crate::ui::components::context_menu_paint::build_context_menu_canvas(
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
                scene_ui_pipeline,
                fg_pipeline,
                fg_color_pipeline,
                fg_sampler,
                atlas,
                color_atlas,
                atlas_texture.texture(),
                color_atlas_texture.texture(),
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
            ref scene_ui_pipeline,
            ref mut font,
            ref mut atlas,
            ref mut color_atlas,
            ref atlas_texture,
            ref color_atlas_texture,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            ref mut color_glyphs_scratch,
            ref mut dots_scratch,
            ref mut ui_rects_scratch,
            ref mut ui_slab,
            ref mut overlay_cells_scratch,
            ref mut overlay_glyphs_scratch,
            ref mut overlay_color_glyphs_scratch,
            ref mut overlay_ui_rects_scratch,
            window_focused,
            hover_chrome_btn,
            ref process_panel,
            ref cc_usage,
            ref settings_panel,
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
            scene_ui_pipeline,
            fg_sampler,
            atlas_texture.texture(),
            color_atlas_texture.texture(),
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
            // Safe to refill in place: the pool is this window's own,
            // and a window never has two frames in flight — so the
            // frame that last read these buffers has completed.
            Some(&mut wr.instance_pool),
            ui_slab,
        );
        let t_canvas0 = std::time::Instant::now();
        // Dev panel + ContextMenu canvases (same shape as render_layout).
        let chrome_cell_w = font.cell_w as f32;
        let chrome_cell_h = font.cell_h as f32;
        let chrome_ascent = font.ascent as f32;
        let viewport_px = [width_px as f32, height_px as f32];
        if let Some(dev_state) = self.dev_panel_state.as_ref()
            && dev_state.visible {
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
                encode_canvas_into(
                    &canvas, target, &cmd,
                    scene_ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                    atlas, color_atlas, atlas_texture.texture(), color_atlas_texture.texture(), device, font,
                    None, &viewport_px,
                    chrome_cell_w, chrome_cell_h, chrome_ascent,
                    false,
                );
            }
        if let Some(menu_state) = self.context_menu_state.as_ref() {
            let canvas = crate::ui::components::context_menu_paint::build_context_menu_canvas(
                menu_state, width_px, height_px,
                chrome_cell_w, chrome_cell_h,
            );
            encode_canvas_into(
                &canvas, target, &cmd,
                scene_ui_pipeline, fg_pipeline, fg_color_pipeline, fg_sampler,
                atlas, color_atlas, atlas_texture.texture(), color_atlas_texture.texture(), device, font,
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
            cmd.waitUntilCompleted();
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
    scene_ui_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas_texture: &ProtocolObject<dyn MTLTexture>,
    color_atlas_texture: &ProtocolObject<dyn MTLTexture>,
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
    // Reused across frames so encoding the rects allocates nothing in
    // a steady state.
    ui_slab: &mut Vec<u8>,
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

    // One pass for the whole frame. Clear to SIDEBAR_BG first: the
    // chrome strip above the grid is not covered by any opaque cell,
    // so it relies on the clear to reset every frame. (Load-by-default
    // was tried once, 2126cda, and let the strip's antialiased glyphs
    // accumulate over their own edges.) `clear_bg` is kept as the hook
    // for a per-frame decision.
    //
    // The frame used to be eight passes, one per kind of instance, each
    // loading the whole target and storing it again. It is now one
    // scene: the same instances in one slab, in layers that keep the
    // order the passes drew in -- see `frame_scene`.
    let _ = clear_bg;
    let mut layers = [golia_ui_core::scene::Layer::default(); crate::frame_scene::FRAME_LAYERS];
    let frame = crate::frame_scene::FrameInstances {
        cells,
        dots,
        ui_rects,
        glyphs,
        color_glyphs,
        overlay_cells,
        overlay_ui_rects,
        overlay_glyphs,
    };
    let clip = golia_ui_core::units::RectPx::new(0.0, 0.0, viewport_w, viewport_h);
    let built = crate::frame_scene::build(&frame, clip, ui_slab, &mut layers);
    let slab_buffer = inst!(0, &ui_slab[..built.bytes]);

    let pass = { MTLRenderPassDescriptor::new() };
    unsafe {
        let color = pass.colorAttachments().objectAtIndexedSubscript(0);
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
    let enc = cmd.renderCommandEncoderWithDescriptor(&pass).expect("frame encoder");
    unsafe { enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1) };
    let Some(buf) = &slab_buffer else {
        // Nothing in the scene: the clear is the frame.
        enc.endEncoding();
        return;
    };
    use golia_ui_core::scene::Kind;
    let (tw, th) = (target.width(), target.height());
    for layer in &layers[..built.layers] {
        enc.setScissorRect(scissor_for(layer.clip, tw, th));
        for kind in Kind::ALL {
            let run = layer.runs[kind as usize];
            if run.count == 0 {
                continue;
            }
            let pipeline = match kind {
                Kind::Rect => bg_pipeline,
                Kind::Circle => dot_pipeline,
                Kind::Glyph => fg_pipeline,
                Kind::ColorGlyph => fg_color_pipeline,
                Kind::UiRect => scene_ui_pipeline,
                // nothing produces images yet, and there is no pipeline
                Kind::Image => continue,
            };
            enc.setRenderPipelineState(pipeline);
            unsafe {
                enc.setVertexBuffer_offset_atIndex(Some(buf), run.offset as usize, 0);
                match kind {
                    Kind::Glyph => {
                        enc.setFragmentTexture_atIndex(Some(atlas_texture), 0);
                        enc.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
                    }
                    Kind::ColorGlyph => {
                        enc.setFragmentTexture_atIndex(Some(color_atlas_texture), 0);
                        enc.setFragmentSamplerState_atIndex(Some(fg_sampler), 0);
                    }
                    _ => {}
                }
                enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    run.count as usize,
                );
            }
        }
    }
    enc.endEncoding();
}

/// A layer's clip as Metal's scissor, held inside the target: Metal
/// rejects a scissor that reaches past the attachment.
fn scissor_for(clip: golia_ui_core::units::RectPx, tw: usize, th: usize) -> MTLScissorRect {
    let x0 = (clip.x.max(0.0).floor() as usize).min(tw);
    let y0 = (clip.y.max(0.0).floor() as usize).min(th);
    let x1 = ((clip.x + clip.w).max(0.0).ceil() as usize).min(tw);
    let y1 = ((clip.y + clip.h).max(0.0).ceil() as usize).min(th);
    MTLScissorRect { x: x0, y: y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
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



/// Build a Shared-storage MTLBuffer over `bytes`.  Returns `None`
/// for an empty payload so the caller can skip the bind/draw.
/// Microseconds spent in `newBufferWithBytes` and bytes handed to it,
/// since the last `take_instance_buffer_cost`.
///
/// Every pass allocates a *fresh* `MTLBuffer` for its instances on
/// every frame — several megabytes a frame across the passes — which
/// is exactly what a per-frame hot path must not do.
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


/// The glyph atlas's texture: the half of the atlas that is Metal's.
///
/// The atlas packs and evicts on the CPU and writes each new glyph
/// through `AtlasSink` the moment it is rasterised; this is what it
/// writes into, and what the glyph passes bind.  The renderer keeps one
/// handle and the atlas the other.
pub struct MetalAtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    width: u32,
    height: u32,
    bpp: u32,
}

impl MetalAtlasTexture {
    /// An R8 texture for the mono atlas, or a BGRA8 one for colour.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
    ) -> Result<std::sync::Arc<Self>, String> {
        let (format, bpp) = if color {
            (MTLPixelFormat::BGRA8Unorm, 4u32)
        } else {
            (MTLPixelFormat::R8Unorm, 1u32)
        };
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format,
                width as usize,
                height as usize,
                false,
            )
        };
        // Managed: CPU writes via replaceRegion, GPU reads.  On Apple
        // Silicon Shared would also work and skip the synchronize step,
        // but Managed is portable across Intel and Apple Silicon and the
        // difference is irrelevant for an atlas written on a miss only.
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        descriptor.setUsage(objc2_metal::MTLTextureUsage::ShaderRead);
        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;
        Ok(std::sync::Arc::new(Self { texture, width, height, bpp }))
    }

    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

impl crate::glyph_atlas::AtlasSink for MetalAtlasTexture {
    fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn bytes_per_pixel(&self) -> u32 {
        self.bpp
    }

    fn upload(&self, bytes: &[u8], w: u32, h: u32, x: u32, y: u32) {
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: x as usize, y: y as usize, z: 0 },
            size: objc2_metal::MTLSize { width: w as usize, height: h as usize, depth: 1 },
        };
        unsafe {
            let ptr = NonNull::new(bytes.as_ptr() as *mut c_void).unwrap();
            self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                ptr,
                (w * self.bpp) as usize,
            );
        }
    }
}

/// A glyph atlas over a new Metal texture, and the texture to bind it by.
pub fn new_atlas(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
    color: bool,
    fonts: std::sync::Arc<crate::font_cache::CoreTextFontTable>,
) -> Result<(GlyphAtlas, std::sync::Arc<MetalAtlasTexture>), String> {
    let texture = MetalAtlasTexture::new(device, width, height, color)?;
    let atlas = GlyphAtlas::new(texture.clone(), fonts)?;
    Ok((atlas, texture))
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
/// no allocation at all, which is what is asked of a
/// per-frame path in the first place.
///
/// **Safety contract**: refilling in place is only sound if the GPU is
/// done with the previous frame's contents.  On the IOSurface path the
/// pool belongs to one window (`WindowRender::instance_pool`) and a
/// window never starts a second frame while its first is in flight, so
/// it is.  It was one pool for every window back when each frame ended
/// in `waitUntilCompleted`; once frames were left running, two windows
/// painting at once overwrote each other's instances.  The live
/// `CAMetalLayer` path only waits until *scheduled*, so it keeps
/// allocating per frame and passes `None`.
pub struct InstanceBufferPool {
    slots: [Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>>; INSTANCE_SLOTS],
    caps: [usize; INSTANCE_SLOTS],
}

/// One slot: the IOSurface path uploads a frame as one scene slab.
pub const INSTANCE_SLOTS: usize = 1;

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
/// ```text
/// final.rgb = src.rgb * src.a + dst.rgb * (1 - src.a)
/// final.a   = src.a   * src.a + dst.a   * (1 - src.a)
/// ```
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
/// ```text
/// final.rgb = src.rgb * 1 + dst.rgb * (1 - src.a)
/// ```
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


fn build_scene_ui_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    build_ui_pipeline_with(device, library, "scene_ui_rect_vertex")
}


fn build_ui_pipeline_with(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    vertex_fn: &str,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, vertex_fn)?;
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
        //
        // A local slab: this is a headless one-shot, not the frame
        // path, so there is no frame-to-frame buffer to reuse.
        let mut slab: Vec<u8> = Vec::new();
        let cells_bytes = cells_as_bytes(cells, &mut slab);
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
        cmd.waitUntilCompleted();

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
/// The bytes the GPU reads for a run of flat rects, written by the
/// published encoder into a slab the caller reuses.
///
/// Not a reinterpret, for the same reason the rounded rects are not:
/// `RectInstance` is a plain Rust struct with an explicit `encode`, so
/// its field order in memory is the compiler's business and only
/// `encode` says what the layout is.
///
/// This is the per-frame bulk -- thousands of cells -- so the slab is
/// kept between frames and the encode writes in place.
fn cells_as_bytes<'a>(cells: &[CellInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = cells.len() * CellInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, c) in cells.iter().enumerate() {
        let at = i * CellInstance::SIZE;
        c.encode(&mut slab[at..at + CellInstance::SIZE]);
    }
    &slab[..need]
}

/// SAFETY: same reasoning as `cells_as_bytes` — `GlyphInstance` is
/// `#[repr(C)]` with no padding.
/// The bytes the GPU reads for a run of rounded rects, written by the
/// published encoder into a slab the caller reuses.
///
/// Not a reinterpret of the slice: `UiRectInstance` is a plain Rust
/// struct with an explicit `encode`, not a `repr(C)` mirror of a
/// shader struct, so its field order in memory is the compiler's
/// business and only `encode` says what the layout is. That is the
/// property this move was for -- one declaration of the layout, and
/// the shader reads what it writes.
///
/// The slab grows to fit and is kept between frames, so a steady state
/// allocates nothing.
fn ui_rects_as_bytes<'a>(rects: &[UiRectInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = rects.len() * UiRectInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, r) in rects.iter().enumerate() {
        let at = i * UiRectInstance::SIZE;
        r.encode(&mut slab[at..at + UiRectInstance::SIZE]);
    }
    &slab[..need]
}

/// The bytes the GPU reads for a run of glyphs, written by the
/// published encoder into a slab the caller reuses.
///
/// Same rule as the flat and rounded rects: `GlyphInstance` is a plain
/// Rust struct with an explicit `encode`, so only `encode` says what
/// the layout is.
fn glyphs_as_bytes<'a>(glyphs: &[GlyphInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = glyphs.len() * GlyphInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, g) in glyphs.iter().enumerate() {
        let at = i * GlyphInstance::SIZE;
        g.encode(&mut slab[at..at + GlyphInstance::SIZE]);
    }
    &slab[..need]
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

        let mut slab: Vec<u8> = Vec::new();
        let glyph_bytes = glyphs_as_bytes(glyphs, &mut slab);
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
        cmd.waitUntilCompleted();

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

/// Free-function `encode_canvas` — composes with caller's
/// destructured `&mut self` borrows (render_layout style).  The
/// method wrapper below is for tests / one-shot callers.
#[allow(clippy::too_many_arguments)]
pub fn encode_canvas_into(
    canvas: &crate::ui::core::canvas::Canvas,
    target: &ProtocolObject<dyn MTLTexture>,
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    scene_ui_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_color_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    atlas_texture: &ProtocolObject<dyn MTLTexture>,
    color_atlas_texture: &ProtocolObject<dyn MTLTexture>,
    device: &ProtocolObject<dyn MTLDevice>,
    font: &mut FontCache,
    clear_color: Option<MTLClearColor>,
    viewport_px: &[f32; 2],
    chrome_cell_w: f32,
    chrome_cell_h: f32,
    chrome_ascent: f32,
    ui_font: bool,
) {
    // Local to this call: a canvas is encoded a few times a frame at
    // most, and threading the renderer's slab through every caller
    // would be more plumbing than the allocation is worth. The frame
    // path, which runs every frame, uses the reused one.
    let mut ui_slab: Vec<u8> = Vec::new();
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
            if let (true, Some(c)) = (first_pass, clear_color) {
                color.setLoadAction(MTLLoadAction::Clear);
                color.setClearColor(c);
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
                enc.setRenderPipelineState(scene_ui_pipeline);
                let slice = &ui_buf[ui_cursor..ui_cursor + run.count];
                let buf =
                    make_instance_buffer(device, ui_rects_as_bytes(slice, &mut ui_slab));
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
                let buf =
                    make_instance_buffer(device, glyphs_as_bytes(slice, &mut ui_slab));
                if let Some(b) = &buf {
                    unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
                }
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(atlas_texture), 0);
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
                let buf =
                    make_instance_buffer(device, glyphs_as_bytes(slice, &mut ui_slab));
                if let Some(b) = &buf {
                    unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
                }
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(color_atlas_texture), 0);
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

    if let (true, Some(cc)) = (first_pass, clear_color) {
        let pass = { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setClearColor(cc);
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
            &self.scene_ui_pipeline, &self.fg_pipeline, &self.fg_color_pipeline, &self.fg_sampler,
            &mut self.atlas, &mut self.color_atlas,
            self.atlas_texture.texture(), self.color_atlas_texture.texture(), &self.device, &mut self.font,
            clear_color, viewport_px,
            chrome_cell_w, chrome_cell_h, chrome_ascent,
            ui_font,
        );
    }



    /// Draw one published instance and hand back the pixels.
    pub fn render_scene_rect(
        &mut self,
        width: u32,
        height: u32,
        rect: golia_ui_core::scene::UiRectInstance,
    ) -> Result<Vec<u8>, String> {
        use golia_ui_core::scene::Encode;
        let mut bytes = vec![0u8; golia_ui_core::scene::UiRectInstance::SIZE];
        rect.encode(&mut bytes);
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        self.draw_ui_instances(
            width, height, &bytes, 1,
            &self.scene_ui_pipeline.clone(), &viewport_px,
        )
    }

    /// One render pass of `count` instances through `pipeline`, read
    /// back as BGRA bytes.
    fn draw_ui_instances(
        &mut self,
        width: u32,
        height: u32,
        instances: &[u8],
        count: u32,
        pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
        viewport_px: &[f32; 2],
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

        let buf = make_instance_buffer(&self.device, instances);
        let pass = MTLRenderPassDescriptor::new();
        unsafe {
            let color = pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(ProtocolObject::from_ref(&*texture)));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 });
            color.setStoreAction(MTLStoreAction::Store);
        }
        if let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) {
            enc.setRenderPipelineState(pipeline);
            if let Some(b) = &buf {
                unsafe { enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0) };
            }
            unsafe {
                enc.setVertexBytes_length_atIndex(
                    NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap(),
                    std::mem::size_of::<[f32; 2]>(),
                    1,
                );
                enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle, 0, 6, count as usize,
                );
            }
            enc.endEncoding();
        }

        let blit = cmd.blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();

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
        cmd.waitUntilCompleted();

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
    use crate::frame_build::glyph_resolve::{resolve_cell_glyph_routed, resolve_cluster_glyph};
    use crate::frame_build::text_run::preedit_placements;
    use crate::frame_build::color::rgba8_of_f32;
    use crate::frame_build::frame::{
        attention_scrim, DRAG_SOURCE_SCRIM, EMPTY_SEAT_SCRIM, PARKED_SCRIM, RESTING_SCRIM,
        UNFOCUSED_SCRIM,
    };

    /// Phase 9 (extended) — pure-Rust SSIM (Structural Similarity
    /// Index) over the luma channel.  No new deps — `png` is already
    /// in the workspace, the rest is arithmetic over RGBA bytes.
    ///
    /// 8×8 non-overlapping windows; standard SSIM weights
    /// (`k1 = 0.01`, `k2 = 0.03`, `L = 255`).  Luma per pixel uses
    /// BT.709 weights (`0.2126 R + 0.7152 G + 0.0722 B`).  Returns
    /// the unweighted mean SSIM across every window — 1.0 = identical,
    /// `< 0.98` is the project's visual-regression gate.
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

    /// Phase 9 — the SSIM gate.
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
    /// The SSIM gate has to be able to fail.
    ///
    /// Every snapshot reading is a number near 1.0, and a comparison
    /// that silently compares an image with itself reads exactly the
    /// same.  Two images that differ have to come out under the
    /// threshold the gate uses.
    #[test]
    fn ssim_separates_two_different_images() {
        let (w, h) = (64u32, 64u32);
        let n = (w * h * 4) as usize;
        let flat = vec![128u8; n];
        assert_eq!(ssim_luma(&flat, &flat, w, h), 1.0, "an image against itself");

        // Same mean, different structure: half the pixels dark, half
        // bright.  A gate that only looked at averages would miss it.
        let mut split = vec![0u8; n];
        for (i, px) in split.chunks_mut(4).enumerate() {
            let v = if (i / w as usize).is_multiple_of(2) { 30 } else { 226 };
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let s = ssim_luma(&flat, &split, w, h);
        assert!(s < 0.98, "two different images must fall under the gate, got {s}");
    }

    /// Write a baseline PNG — but only when recording one.
    ///
    /// These tests used to write their own baseline on every run,
    /// including the run that had just compared against it.  A render
    /// that drifted therefore updated the file it was being judged
    /// by, and the SSIM gate could never fail twice.  Recording is now
    /// something you ask for.
    fn write_snapshot_png(path: &std::path::Path, rgba: &[u8], w_px: u32, h_px: u32) {
        if std::env::var("MARSPOT_FONT_SNAPSHOT").as_deref() != Ok("1") {
            return;
        }
        let file = std::fs::File::create(path).expect("create png");
        let buf = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(buf, w_px, h_px);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(rgba).expect("png data");
    }

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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
    ///
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
    /// The SSIM > 0.98 gate lives in a follow-up — this commit only
    /// fixes the rendered bytes.
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
    ///
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y_pt), source)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
        // The colour is `palette::CURSOR` because `OSC 12 ; ? ST`
        // asks for it; the alpha stays here, because how solid the
        // block looks is a drawing decision and not part of the
        // answer.
        let (cr, cg, cb) = marspot_term::palette::CURSOR;
        let cursor_color = Color::rgba(cr, cg, cb, 0.90);
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
                .text(Length::Pt(pad_x_pt), Length::Pt(y), line)
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
        write_snapshot_png(&out_path, &rgba, w_px, h_px);
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
                right_badge: "", agent_tui: false, cwd: "",
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
            for px in fresh.as_chunks::<4>().0 {
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

    /// Two windows must not refill each other's instance buffers.
    ///
    /// A frame is committed and left running now, so the instances
    /// window A's frame is reading are still live when window B is
    /// built.  One pool for every window meant B's fill overwrote them
    /// and A's in-flight frame drew B's content — at B's coordinates,
    /// so in A's top-left corner, for one frame.  Reported 2026-09-27
    /// as a second window appearing in the corner of the first and
    /// vanishing again.
    ///
    /// Asserted on the buffers rather than on pixels on purpose: which
    /// of the two frames the GPU finishes first is a race, so a pixel
    /// test would pass on a good day with the bug still in.
    #[test]
    fn each_window_fills_its_own_instance_buffers() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;

        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => return, // no Metal on this machine
        };
        let device = renderer.device().to_owned();
        let target = |w: usize, h: usize| {
            let desc = unsafe {
                objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    TARGET_FORMAT, w, h, false,
                )
            };
            desc.setUsage(objc2_metal::MTLTextureUsage::RenderTarget);
            device.newTextureWithDescriptor(&desc)
        };
        let (Some(target_a), Some(target_b)) = (target(320, 160), target(200, 120)) else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_dims();
        let mut grid = Grid::new(20, 6);
        grid.set_cell(0, 0, Cell { ch: 'x', attrs: Default::default() });
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
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 1,
        };

        let mut wr_a = WindowRender::new();
        let mut wr_b = WindowRender::new();
        let layout_a = Layout::build(320.0, 160.0, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
        let layout_b = Layout::build(200.0, 120.0, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
        renderer.render_layout_to_texture(
            &mut wr_a, &target_a, &layout_a, std::slice::from_ref(&view), &[], 0,
        );
        renderer.render_layout_to_texture(
            &mut wr_b, &target_b, &layout_b, std::slice::from_ref(&view), &[], 0,
        );

        let mut compared = 0;
        for slot in 0..INSTANCE_SLOTS {
            let (Some(a), Some(b)) =
                (&wr_a.instance_pool.slots[slot], &wr_b.instance_pool.slots[slot])
            else {
                continue;
            };
            assert_ne!(
                a.contents().as_ptr(),
                b.contents().as_ptr(),
                "slot {slot} is the same memory for both windows",
            );
            compared += 1;
        }
        assert!(compared > 0, "no slot was filled by either window — nothing was compared");
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


    /// The resolver hands back an atlas entry for a cluster, and
    /// caches it.
    ///
    /// Between "CoreText draws the pair" and "the render loop asks for
    /// it" sits this function, and nothing tested it: the pixel test
    /// below calls the rasteriser directly, and the grid tests never
    /// reach the atlas.  A cluster that failed here would simply draw
    /// nothing, silently, for every cell holding one.
    #[test]
    fn the_resolver_caches_a_cluster_entry() {
        let Ok(mut font) = crate::font_cache::FontCache::build() else {
            eprintln!("skipping: no font stack");
            return;
        };
        let Ok(r) = MetalRenderer::new_headless() else {
            eprintln!("skipping: no Metal device");
            return;
        };
        let Ok(mut atlas) = new_atlas(&r.device, 512, 512, false, font.font_table()).map(|(a, _)| a) else {
            eprintln!("skipping: atlas would not allocate");
            return;
        };
        let metrics = SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 };

        let first = resolve_cluster_glyph(&mut atlas, &mut font, "e\u{301}", false, false, metrics);
        assert!(first.is_some(), "the resolver returned no entry for a cluster");

        // Asked again, it must be the same slot — a screen full of one
        // cluster has to cost one raster, not one per cell.
        let again = resolve_cluster_glyph(&mut atlas, &mut font, "e\u{301}", false, false, metrics);
        assert_eq!(
            format!("{:?}", first.map(|e| (e.u0, e.v0))),
            format!("{:?}", again.map(|e| (e.u0, e.v0))),
            "the same cluster took a second atlas slot"
        );

        // And a different cluster is a different slot, or the key is
        // not carrying the text.
        let other = resolve_cluster_glyph(
            &mut atlas, &mut font, "\u{1f1ef}\u{1f1f5}", false, false, metrics,
        );
        assert!(other.is_some());
        assert_ne!(
            format!("{:?}", first.map(|e| (e.u0, e.v0))),
            format!("{:?}", other.map(|e| (e.u0, e.v0))),
            "two different clusters share a slot; the key ignores the text"
        );
    }

    /// A cluster rasterises to something other than its base alone.
    ///
    /// The grid now keeps `e` + U+0301 whole, and the renderer asks
    /// CoreText to draw the pair.  If that path were a no-op — wrong
    /// font, empty line, baseline off the bitmap — the cell would
    /// simply look like `e` and every test above would still pass,
    /// because they are all about the grid.  This compares pixels.
    #[test]
    fn a_cluster_draws_differently_from_its_base() {
        let Ok(mut font) = crate::font_cache::FontCache::build() else {
            eprintln!("skipping: no font stack");
            return;
        };
        let metrics = SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 };
        let mut buf_base = vec![0u8; 16 * 32];
        let mut buf_pair = vec![0u8; 16 * 32];

        let (idx, _) = font.resolve_char('e', false, false);
        let ct = font.font(idx).expect("the resolved font is in the table");
        assert!(
            crate::glyph_atlas::rasterise_cluster("e", &ct, metrics, 1, &mut buf_base),
            "the rasteriser refused a one-codepoint cluster"
        );
        assert!(
            crate::glyph_atlas::rasterise_cluster("e\u{301}", &ct, metrics, 1, &mut buf_pair),
            "the rasteriser refused the pair"
        );

        let ink_base: u32 = buf_base.iter().map(|b| *b as u32).sum();
        let ink_pair: u32 = buf_pair.iter().map(|b| *b as u32).sum();
        assert!(ink_base > 0, "even the base drew nothing; the path is dead");
        assert!(
            ink_pair > ink_base,
            "the mark added no ink: base {ink_base}, pair {ink_pair}"
        );

        // And the mark sits above the letter, which is the whole
        // point of letting CoreText place it rather than stacking two
        // rasters.  Row order is taken from the data, not from an
        // argument about Core Graphics' origin: the letter's own ink
        // says which way is up, and the mark has to be on the other
        // side of its first row.
        let row_ink = |b: &[u8]| -> Vec<u32> {
            (0..32)
                .map(|r| b[r * 16..(r + 1) * 16].iter().map(|v| *v as u32).sum())
                .collect()
        };
        let base_rows = row_ink(&buf_base);
        let pair_rows = row_ink(&buf_pair);
        let base_first = base_rows.iter().position(|v| *v > 0).expect("the base drew nothing");
        let added: Vec<usize> = (0..32).filter(|r| pair_rows[*r] > base_rows[*r]).collect();
        assert!(!added.is_empty(), "the pair added ink to no row");
        assert!(
            added.iter().all(|r| *r < base_first),
            "the mark is not clear of the letter: the letter starts at row \
             {base_first} and the added rows are {added:?}"
        );
        assert!(
            added.iter().all(|r| base_rows[*r] == 0),
            "the mark landed on top of the letter rather than beside it"
        );
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
                color: rgba8_of_f32([1.0, 0.0, 0.0, 1.0]),
            },
            CellInstance {
                origin: [2.0, 0.0],
                size: [2.0, 4.0],
                color: rgba8_of_f32([0.0, 1.0, 0.0, 1.0]),
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
        use crate::frame_build::glyph_resolve::text_glyph_key;
        use core_text::font::new_from_name;

        let r = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("skipping: no Metal device");
                return;
            }
        };

        let font = new_from_name("Menlo", 13.0).expect("Menlo");
        let (mut atlas, atlas_texture) = new_atlas(
            &r.device,
            256,
            256,
            false,
            crate::font_cache::CoreTextFontTable::single(font.clone()),
        )
        .expect("atlas");

        let mut cg_glyph: core_graphics::font::CGGlyph = 0;
        let cu: u16 = b'A' as u16;
        unsafe {
            font.get_glyphs_for_characters(&cu, &mut cg_glyph, 1);
        }
        assert!(cg_glyph != 0);

        let entry = atlas
            .get_or_rasterize(
                text_glyph_key(0, cg_glyph, font.pt_size()),
                SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 },
                1,
            )
            .expect("rasterise A");
        let (atlas_w, atlas_h) = atlas.dims();

        // Two glyph instances, not one. A layout that strides by the
        // wrong number of bytes draws the first correctly and reads
        // the second from the wrong place -- the same trap that put a
        // green cell at G=0 when the flat rects moved, and one
        // instance cannot see it.
        //
        // Drawn at (8, 8) and (36, 8) with the atlas's pixel size and
        // a fully opaque white tint; the atlas's R8 alpha modulates
        // through to the readable colour.
        let one = GlyphInstance {
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
            color: rgba8_of_f32([1.0, 1.0, 1.0, 1.0]),
        };
        let glyphs = vec![one, GlyphInstance { origin: [36.0, 8.0], ..one }];

        let bytes = r
            .render_glyphs_fg_offscreen(64, 64, atlas_texture.texture(), &glyphs, (0.0, 0.0, 0.0, 1.0))
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

        // And the second one, which is the instance that a wrong
        // stride loses.
        let mut second = 0u8;
        for y in 8..(8 + entry.px_h as usize) {
            for x in 36..(36 + entry.px_w as usize) {
                let off = (y * 64 + x) * 4;
                let lum = ((bytes[off] as u16
                    + bytes[off + 1] as u16
                    + bytes[off + 2] as u16)
                    / 3) as u8;
                if lum > second {
                    second = lum;
                }
            }
        }
        assert!(
            second > 100,
            "the second instance never drew: stride, got max={second}"
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
        let grid = crate::grid::Grid::new(10, 4);
        let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
        let view = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
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
        // Alphas are eight bits now, so the scrim is compared as the
        // byte it becomes rather than as the float it was written as.
        let want = (DRAG_SOURCE_SCRIM * 255.0).round() as u8;
        let scrims: Vec<u8> = overlay_rects
            .iter()
            .filter(|r| r.fill.r == 0 && r.fill.a > 0)
            .map(|r| r.fill.a)
            .collect();
        assert!(
            scrims.contains(&want),
            "a dragged pane wears the drag scrim even while focused, got {scrims:?} \
             (wanted {want})"
        );
        const _: () = assert!(DRAG_SOURCE_SCRIM > EMPTY_SEAT_SCRIM);
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
        const _: () = assert!(
            UNFOCUSED_SCRIM < RESTING_SCRIM && RESTING_SCRIM < PARKED_SCRIM,
            "the ladder has to be monotonic or it says nothing"
        );
        const _: () = assert!(
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
        let mut atlas =
            new_atlas(&device, 512, 512, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 64, 64, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
        let grid = crate::grid::Grid::new(10, 4);
        let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
        let mk = |focused: bool, dormant: bool, recede: u32| SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused,
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[], search_overlay: None, seq: 0,
            dormant, recede, scrim: attention_scrim(focused, recede),
        };
        let mut scrim_alphas = |view: SessionView| -> Vec<u8> {
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
                .filter(|r| r.fill.r == 0 && r.fill.a > 0)
                .map(|r| r.fill.a)
                .collect()
        };
        // Alphas are eight bits on the way to the GPU, so the
        // constants are compared as the bytes they become. Comparing
        // the float back out and asking for 1e-6 asks the byte to
        // carry a precision it does not have: 0.75 comes back as
        // 191/255 = 0.7490196.
        let byte = |a: f32| (a * 255.0).round() as u8;
        // Focused and live: nothing over it at all.
        let focused = scrim_alphas(mk(true, false, 0));
        assert!(
            !focused.iter().any(|a| *a == byte(UNFOCUSED_SCRIM)),
            "the focused pane gets no attention scrim, got {focused:?}"
        );
        // Unfocused and parked: one scrim, the parked one.
        let parked = scrim_alphas(mk(false, false, 2));
        assert!(
            parked.iter().any(|a| *a == byte(PARKED_SCRIM)),
            "a parked pane recedes to the parked tier, got {parked:?}"
        );
        // Parked AND an empty seat: still one scrim, the deeper of
        // the two.
        let both = scrim_alphas(mk(false, true, 2));
        let deep = both.iter().filter(|a| **a >= byte(EMPTY_SEAT_SCRIM)).count();
        assert_eq!(deep, 1, "exactly one scrim, got {both:?}");
        assert!(both.iter().any(|a| *a == byte(PARKED_SCRIM)));
    }

    /// The shader reads what the encoder writes.
    ///
    /// There used to be two paths and a test that drew the same rects
    /// through both: if an offset disagreed, the two bitmaps stopped
    /// matching. There is one path now, so the comparison has nothing
    /// to compare against and the question needs asking directly --
    /// put a distinct value in every field and look at the pixels it
    /// produces.
    ///
    /// Each assertion names a field, so a layout that slips by four
    /// bytes says which one moved rather than "the picture changed".
    #[test]
    fn the_shader_reads_what_the_encoder_writes() {
        let Ok(mut r) = MetalRenderer::new_headless() else {
            eprintln!("skipping: no Metal device on this host");
            return;
        };
        let (w, h) = (96u32, 96u32);
        let px = |buf: &[u8], x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            // BGRA, which is what the target format is.
            (buf[i + 2], buf[i + 1], buf[i], buf[i + 3])
        };
        let out = r
            .render_scene_rect(
                w,
                h,
                golia_ui_core::scene::UiRectInstance {
                    origin: [24.0, 24.0],
                    size: [48.0, 48.0],
                    fill: golia_ui_core::Rgba8::rgba(200, 60, 40, 255),
                    border: golia_ui_core::Rgba8::rgba(40, 200, 90, 255),
                    radius: 0.0,
                    border_width: 4.0,
                    shadow_offset: [0.0, 0.0],
                    shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
                    shadow_blur: 0.0,
                },
            )
            .expect("one rect");

        // `origin` and `size`: inside is painted, outside is not.
        assert_eq!(px(&out, 48, 48).0, 200, "fill red at the centre");
        // The pass clears to opaque black, which is also why a black
        // shadow on this target is invisible.
        assert_eq!(px(&out, 4, 4), (0, 0, 0, 255), "nothing outside the rect");
        // `fill`: the centre is the fill colour, not the border's.
        assert_eq!(px(&out, 48, 48), (200, 60, 40, 255), "fill colour");
        // `border` and `border_width`: the shader strokes inside, so
        // two pixels in from the edge is still border.
        assert_eq!(px(&out, 26, 48), (40, 200, 90, 255), "border colour");
        // ... and six in is past a four-pixel stroke.
        assert_eq!(px(&out, 30, 48), (200, 60, 40, 255), "border is 4px wide");
    }

    /// The focus ring survives the neighbours' scrims.
    ///
    /// The ring is painted in the BG pass at the seam, and the seam on
    /// Where the composition lands: clusters, widths, wrapping, and
    /// the bottom edge.
    #[test]
    fn a_composition_wraps_and_stops_at_the_last_row() {
        // Plain, from the cursor.
        assert_eq!(
            preedit_placements("abc", (2, 0), 10, 4),
            vec![(0, 2, 1, "a"), (0, 3, 1, "b"), (0, 4, 1, "c")]
        );
        // A wide cluster takes two cells and wraps whole rather than
        // being split across the edge.
        assert_eq!(
            preedit_placements("中文", (9, 0), 10, 4),
            vec![(1, 0, 2, "中"), (1, 2, 2, "文")]
        );
        // A structured composition's newline goes to the next row.
        assert_eq!(
            preedit_placements("a\nb", (0, 0), 10, 4),
            vec![(0, 0, 1, "a"), (1, 0, 1, "b")]
        );
        // Out of vertical room: what fits is placed, the rest is the
        // candidate window's job.
        assert_eq!(preedit_placements("ab", (9, 3), 10, 4), vec![(3, 9, 1, "a")]);
        assert!(preedit_placements("a", (0, 0), 0, 0).is_empty());
    }

    /// The cells a composition covers must be left empty by the grid
    /// loops.
    ///
    /// BG and FG are separate passes, so the preedit's opaque
    /// background covers only the cells' BACKGROUND; the terminal's
    /// own glyphs are drawn afterwards and land back on top of the
    /// composition.  Reported against a claudecode placeholder
    /// ("press up to edit queued messages") that stayed legible
    /// underneath the pinyin being typed over it (2026-09-23).
    #[test]
    fn a_composition_hides_the_cells_it_covers() {
        use crate::grid::{Cell, Grid};
        use crate::layout::Layout;

        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

        // Six columns of text under a three-cell composition typed at
        // the start of the row — the placeholder's own shape.
        let mut grid = Grid::new(10, 4);
        for (i, ch) in "abcdef".chars().enumerate() {
            grid.set_cell(i as u16, 0, Cell { ch, attrs: Default::default() });
        }
        let layout = Layout::build(
            font.cell_w * 10.0,
            font.cell_h * 4.0,
            0.0, 0.0, 0.0, 1, 1,
            font.cell_w,
            font.cell_h,
        );
        let mk = |preedit: &'static str| SessionView {
            grid: &grid,
            view_offset: 0,
            // Off, so the cursor's own glyph re-emit is not what this
            // measures.
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: preedit,
            update_pending: false,
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let mut run = |v: &SessionView| -> Vec<GlyphInstance> {
            let mut glyphs: Vec<GlyphInstance> = Vec::new();
            build_instances(
                &layout, std::slice::from_ref(v), &[], 0, true,
                None, None, None, None, None, None, None, None,
                &mut font, &mut atlas, &mut color_atlas,
                &mut Vec::new(), &mut glyphs, &mut Vec::new(), &mut Vec::new(),
                &mut Vec::new(), &mut Vec::new(),
                &mut Vec::new(), &mut Vec::new(), &mut Vec::new(),
            );
            glyphs
        };
        let plain = run(&mk(""));
        let composing = run(&mk("tfg"));
        assert_eq!(plain.len(), 6, "six letters, six glyphs");
        assert_eq!(
            composing.len(),
            6,
            "three letters left + three composed: the covered cells must emit nothing, \
             or the text underneath is drawn over the composition"
        );
        // And the three that remain are the ones to the RIGHT of the
        // composition: everything at x < 3 cells belongs to it.
        let under = 3.0 * font.cell_w as f32;
        let over_composition = composing.iter().filter(|g| g.origin[0] < under).count();
        assert_eq!(over_composition, 3, "one glyph per composed cell, none from the grid");
    }

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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

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
            right_badge: "", agent_tui: false, cwd: "",
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

        let ring = rgba8_of_f32([0.72, 0.76, 0.82, 1.0]);
        let ring_at: Vec<usize> = overlay_ui
            .iter()
            .enumerate()
            .filter(|(_, r)| r.fill == ring)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(ring_at.len(), 8, "expected the 8 ring rects in the overlay pass, got {ring_at:?}");

        // Order is what makes it visible: every scrim must be pushed
        // before the ring, or the ring is dimmed again.
        let last_scrim = overlay_ui
            .iter()
            .rposition(|r| {
                r.fill.a > 0 && r.fill.r == 0 && r.fill.g == 0 && r.fill.b == 0
            })
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

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
            right_badge: "", agent_tui: false, cwd: "",
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
        let mut atlas =
            new_atlas(&device, 512, 512, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 512, 512, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

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
            right_badge: "", agent_tui: false, cwd: "",
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
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
            right_badge: "", agent_tui: false, cwd: "",
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
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
            right_badge: "", agent_tui: false, cwd: "",
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
            cwd: "",
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
            // Eight bits now, so the colour is matched as the byte it
            // becomes rather than as the float it was written as.
            .filter(|c| c.color.r == (HIGHLIGHT_BG.0 * 255.0).round() as u8)
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
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
            right_badge: "", agent_tui: false, cwd: "",
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
            // Eight bits now, so the colour is matched as the byte it
            // becomes rather than as the float it was written as.
            .filter(|c| c.color.r == (HIGHLIGHT_BG.0 * 255.0).round() as u8)
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
        let mut atlas =
            new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
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
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            seq: 0,
        };
        let view_bot = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false, dormant: false,
                                                                                recede: 0,
                scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 2,
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

