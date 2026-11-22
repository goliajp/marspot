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
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat,
    MTLRenderPipelineState, MTLSamplerState,
};
use std::ptr::NonNull;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
#[cfg(test)]
use crate::glyph_atlas::SlotMetrics;
use crate::ui::components::process_panel::ProcessPanelRender;
use crate::ui::components::cc_usage_modal::CcUsageRender;
use crate::ui::components::settings_view::SettingsRender;
use crate::ui::components::shortcuts_view::ShortcutsRender;
use crate::ui::components::layout_view::LayoutModalRender;
use crate::ui::components::context_menu_view::MenuRender;
pub use crate::frame_build::chrome::ChromeShape;
#[cfg(test)]
use crate::render::SessionView;
#[cfg(test)]
use crate::render::HighlightSpan;

#[cfg(test)]
use crate::frame_build::frame::build_instances;

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

/// Metal-backed renderer.  Has the pipeline state for the BG pass
/// (phase 3) but no FG pass yet — `render_cells_bg` is the entry
/// point and `render_cells_bg_offscreen` is its testable cousin.
pub struct MetalRenderer {
    /// The default system device.  One per process; cheap to clone (CFRetain).
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    /// Command submission queue.  One per renderer is fine; Apple's
    /// guidance says queues are heavyweight and should be reused.
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
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
    atlas_texture: std::rc::Rc<MetalAtlasTexture>,
    color_atlas_texture: std::rc::Rc<MetalAtlasTexture>,
    /// Per-frame instance scratch.  Reset at the start of each
    /// render so per-frame allocations stay zero in the
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
    /// F3+1.3 — process-tree panel render data.  `None` = panel
    /// closed (renderer paints nothing).  Pushed by L2 every frame
    /// while the panel is open; cheap because rows are typically
    /// tens of entries.
    process_panel: Option<ProcessPanelRender>,
    cc_usage: Option<CcUsageRender>,
    settings_panel: Option<SettingsRender>,
    shortcuts_panel: Option<ShortcutsRender>,
    /// F3+3.0 / 3.3 — LayoutModal render state.  `Some(_)` when
    /// open, `None` when closed.  See `layout_view::LayoutModalRender`.
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
    context_menu_state: Option<MenuRender>,
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
    /// Handed to every frame left running on the GPU; Metal calls it
    /// when the frame completes.  One block for the renderer's life,
    /// so a frame allocates nothing for it.
    frame_done: Option<FrameDone>,
}

type FrameDone = block2::RcBlock<dyn Fn(NonNull<ProtocolObject<dyn MTLCommandBuffer>>)>;

impl MetalRenderer {
    /// Construct without a view — for unit tests, benches, or a future
    /// offscreen render path.  There is no drawable, but
    /// `render_cells_bg_offscreen` works end-to-end against a
    /// caller-provided MTLTexture.
    pub fn new_headless() -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "MTLDevice.newCommandQueue returned nil".to_string())?;
        let (bg_pipeline, fg_pipeline, fg_color_pipeline, dot_pipeline, scene_ui_pipeline) =
            build_pipelines(&device)?;
        let fg_sampler = build_fg_sampler(&device)?;
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
            process_panel: None, cc_usage: None, settings_panel: None, shortcuts_panel: None, layout_modal_state: None, drop_preview: None, drag_source: None, context_menu_state: None,
            last_render_split: RenderSplit::default(),
            frame_done: None,
        })
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    pub fn window_focused(&self) -> bool {
        self.window_focused
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

    pub fn set_shortcuts_panel(&mut self, data: Option<ShortcutsRender>) {
        self.shortcuts_panel = data;
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

    pub fn set_context_menu(&mut self, state: Option<MenuRender>) {
        self.context_menu_state = state;
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        self.font.cell_dims()
    }

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
        self.atlas.refont(self.font.font_table());
        self.color_atlas.refont(self.font.font_table());
        self.fonts_built_at_scale = want;
        true
    }

    pub fn font_mut(&mut self) -> &mut FontCache {
        &mut self.font
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
    }

    /// Chrome font metrics: (cell_w, cell_h, ascent) in physical
    /// pixels.
    pub fn chrome_font_metrics(&self) -> (f32, f32, f32) {
        (self.font.cell_w as f32, self.font.cell_h as f32, self.font.ascent as f32)
    }
}

pub(crate) fn system_default_device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>, String> {
    // objc2-metal 0.3 wraps the +1 retained pointer (or null) into
    // `Option<Retained<..>>` for us.
    MTLCreateSystemDefaultDevice()
        .ok_or_else(|| "MTLCreateSystemDefaultDevice returned nil — no Metal device".into())
}

/// What an offscreen render for a test or a tool starts from: black,
/// whatever the theme, so its pictures do not depend on one.
const OFFSCREEN_CLEAR: MTLClearColor = MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 };

fn clear_color(c: [f32; 4]) -> MTLClearColor {
    MTLClearColor { red: c[0] as f64, green: c[1] as f64, blue: c[2] as f64, alpha: 1.0 }
}

mod colorspace;
#[cfg(test)]
mod pane_cache_tests;
mod encode;
pub mod pipelines;
mod menu_pass;
pub use menu_pass::MenuBuffers;
#[cfg(test)]
mod sheet;

#[cfg(test)]
pub(crate) mod probe;
use encode::{encode_passes, make_instance_buffer, take_upload_bytes, Drawing};
pub use colorspace::pin_layer_colorspace;
pub use encode::{take_instance_buffer_cost, InstanceBufferPool};
use pipelines::{build_fg_sampler, build_pipelines};
mod window_render;
pub use window_render::{RenderSplit, WindowRender};
mod frame;
mod targets;
pub use targets::{make_readback_texture, make_target_texture, texture_bytes_bgra};
mod atlas_texture;
pub use atlas_texture::{new_atlas, MetalAtlasTexture};
mod instance_bytes;
use instance_bytes::{cells_as_bytes, glyphs_as_bytes, ui_rects_as_bytes};
mod offscreen;
#[cfg(test)]
mod font_snapshot_tests;
#[cfg(test)]
mod font_fallback_snapshot_tests;
#[cfg(test)]
mod scene_snapshot_tests;
#[cfg(test)]
mod instance_buffer_tests;
#[cfg(test)]
mod glyph_tests;
#[cfg(test)]
mod offscreen_tests;
#[cfg(test)]
mod scrim_tests;
#[cfg(test)]
mod build_instances_tests;
#[cfg(test)]
mod fixed_rows_tests;
#[cfg(test)]
mod sheet_tests;
