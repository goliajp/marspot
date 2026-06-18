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
use objc2_foundation::{CGSize, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLSamplerAddressMode, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState, MTLStoreAction, MTLTexture,
};
use core_graphics::color_space::{kCGColorSpaceSRGB, CGColorSpace};
use foreign_types::ForeignType;
use objc2::msg_send;
use objc2_app_kit::NSColor;
use objc2_quartz_core::{kCAGravityTopLeft, CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;
use std::ptr::NonNull;

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
        let key = GlyphKey { font_id: BOX_DRAWING_FONT_ID, glyph: ch as u32 as CGGlyph };
        return atlas.get_or_insert_custom_raster(key, w, h, 1, |buf| {
            rasterize_arms_into_buf(buf, w as usize, h as usize, arms);
        });
    }
    if let Some(shape) = block_element_rects(ch) {
        let w = metrics.cell_w;
        let h = metrics.cell_h;
        let key = GlyphKey { font_id: BOX_DRAWING_FONT_ID, glyph: ch as u32 as CGGlyph };
        return atlas.get_or_insert_custom_raster(key, w, h, 1, |buf| {
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
        GlyphKey { font_id: font_idx as u32, glyph },
        &ct_font,
        metrics,
        n_cells,
    )
}

/// Like `resolve_cell_glyph`, but routes colour glyphs (Apple Color Emoji)
/// to the colour (`BGRA8`) atlas and everything else to the mono (`R8`)
/// atlas.  Returns `(entry, is_color)` so the caller can pick the matching
/// glyph buffer + atlas dims.  Box-drawing / block-element glyphs are always
/// mono (we rasterise those ourselves).
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
    let key = GlyphKey { font_id: font_idx as u32, glyph };
    let n_cells = crate::grid::char_width(ch).max(1) as u16;
    let ct_font = font.font(font_idx).clone();
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
    // `unsafe`); `setColorspace` is reached via msg_send because the
    // `colorspace` property isn't in the objc2-quartz-core binding yet.
    unsafe {
        let cs = CGColorSpace::create_with_name(kCGColorSpaceSRGB).expect(
            "CGColorSpaceCreateWithName(kCGColorSpaceSRGB) cannot fail on supported macOS",
        );
        let cs_ptr = cs.as_ptr() as *mut c_void;
        let _: () = msg_send![layer, setColorspace: cs_ptr];
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
    /// Window-level focus.  Mirror of the AppKit renderer's flag —
    /// drives whether the focused-session cursor is filled or hollow.
    window_focused: bool,
    /// Hovered chrome icon button, if any.  Encoded as u8 to stay
    /// agnostic of the L2-side enum:  0 = sidebar toggle,  1 =
    /// layout picker,  `None` = no hover.  Renderer reads this to
    /// darken the hovered button's BG.
    hover_chrome_btn: Option<u8>,
    /// Top inset in physical pixels — reserved for window chrome
    /// (macOS traffic-light buttons). Single-session callers (mcli)
    /// set this once at `resumed`; the convenience `render(view)`
    /// path forwards it into Layout::build's `top_inset` parameter.
    /// Multi-session callers build their own Layout and ignore this.
    top_inset_phys: f64,
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
    clear_bg_required: bool,
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
        // 2048×2048 R8 atlas = 4 MiB.  Fits ~6000 Menlo 13pt 2× glyphs.
        // 9-grid sessions all feed this single atlas and accumulate
        // bold/italic/underline variants per ASCII character plus CJK
        // and emoji over hours, easily clearing the 1500-glyph mark
        // the original 1024×1024 sized for.  Bounded forever —
        // `get_or_rasterize` does an atomic rebuild on full (drops
        // shelves + clears cache, next frame re-rasterises visible
        // glyphs) so the user never sees silently-blank cells.
        let atlas = GlyphAtlas::new(&device, 2048, 2048)?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let color_atlas = GlyphAtlas::new_color(&device, 1024, 1024)?;

        let layer = unsafe { CAMetalLayer::new() };
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
        unsafe {
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
            glyphs_scratch: Vec::new(),
            color_glyphs_scratch: Vec::new(),
            window_focused: true,
            hover_chrome_btn: None,
            top_inset_phys: 0.0,
            clear_bg_required: true,
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
        // 2048×2048 R8 atlas = 4 MiB.  Fits ~6000 Menlo 13pt 2× glyphs.
        // 9-grid sessions all feed this single atlas and accumulate
        // bold/italic/underline variants per ASCII character plus CJK
        // and emoji over hours, easily clearing the 1500-glyph mark
        // the original 1024×1024 sized for.  Bounded forever —
        // `get_or_rasterize` does an atomic rebuild on full (drops
        // shelves + clears cache, next frame re-rasterises visible
        // glyphs) so the user never sees silently-blank cells.
        let atlas = GlyphAtlas::new(&device, 2048, 2048)?;
        // 1024×1024 BGRA8 colour atlas = 4 MiB.  Holds full-colour emoji
        // (~cell-sized slots) — a small working set, so 1024² is ample
        // and keeps the colour path's footprint to 4 MiB.  Same shelf
        // packer + atomic-rebuild-on-full bound as the mono atlas.
        let color_atlas = GlyphAtlas::new_color(&device, 1024, 1024)?;
        Ok(Self {
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
            glyphs_scratch: Vec::new(),
            color_glyphs_scratch: Vec::new(),
            window_focused: true,
            hover_chrome_btn: None,
            top_inset_phys: 0.0,
            clear_bg_required: true,
        })
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
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
    pub fn mark_bg_clear_required(&mut self) {
        // Dev-only — flash investigation 2026-06-15.  If this fires
        // on every render, my Load-by-default fix isn't actually
        // taking effect and the race window persists.  Demote to
        // lx_debug! once the steady-state Load is confirmed.
        crate::lx_event!("render.bg_clear_required", "flag set");
        self.clear_bg_required = true;
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
    pub fn render(&mut self, view: SessionView) {
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
        self.render_layout(&layout, std::slice::from_ref(&view), &[], 0);
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        self.font.cell_dims()
    }

    /// Caret rect for a single-session render (mcli's path).  Builds
    /// the same 1×1 Layout `render()` uses, then asks the layout where
    /// the focused session's `(col, row)` maps in view-local physical
    /// pixels (top-left, y-down).  Marspot's main loop has a real
    /// multi-cell `Layout` and calls `Layout::caret_view_phys_rect`
    /// directly; the geometry is shared in `Layout` so both binaries
    /// stay in sync.  Returns `None` when the cursor is hidden or the
    /// viewport hasn't been sized yet.
    pub fn focused_caret_view_phys_rect(
        &self,
        view: &SessionView,
    ) -> Option<(f64, f64, f64, f64)> {
        if !view.cursor_visible {
            return None;
        }
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
            unsafe {
                layer.setDrawableSize(CGSize {
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
        let drawable = match unsafe { layer.nextDrawable() } {
            Some(d) => d,
            None => return false,
        };
        let texture = unsafe { drawable.texture() };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
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
    pub fn render_layout(
        &mut self,
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
            window_focused,
            hover_chrome_btn,
            width_px,
            height_px,
            ..
        } = *self;

        cells_scratch.clear();
        glyphs_scratch.clear();
        color_glyphs_scratch.clear();
        dots_scratch.clear();
        ui_rects_scratch.clear();
        build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            hover_chrome_btn,
            font,
            atlas,
            color_atlas,
            cells_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            dots_scratch,
            ui_rects_scratch,
        );

        let layer = layer.as_ref().unwrap();
        let drawable = match unsafe { layer.nextDrawable() } {
            Some(d) => d,
            None => return,
        };
        let texture = unsafe { drawable.texture() };

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
            width_px as f32,
            height_px as f32,
            // CAMetalLayer drawable, same process — no cross-process
            // race possible.  Always Clear for the full hard-fill.
            true,
        );

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
        target: &ProtocolObject<dyn MTLTexture>,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
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
        let clear_bg = self.clear_bg_required;
        self.clear_bg_required = false;
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
            window_focused,
            hover_chrome_btn,
            ..
        } = *self;

        cells_scratch.clear();
        glyphs_scratch.clear();
        color_glyphs_scratch.clear();
        dots_scratch.clear();
        ui_rects_scratch.clear();
        build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            hover_chrome_btn,
            font,
            atlas,
            color_atlas,
            cells_scratch,
            glyphs_scratch,
            color_glyphs_scratch,
            dots_scratch,
            ui_rects_scratch,
        );

        let cmd = match queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
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
            width_px as f32,
            height_px as f32,
            // IOSurface path — cross-process race-free only when Load
            // is used in steady state.  Consume the flag set by
            // `mark_bg_clear_required` (e.g. resize, layout change).
            clear_bg,
        );
        cmd.commit();
        unsafe { cmd.waitUntilCompleted() };
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
) {
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
    let bg_pass = unsafe { MTLRenderPassDescriptor::new() };
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
    let bg_buffer = make_instance_buffer(device, cells_as_bytes(cells));
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
        let dot_pass = unsafe { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = dot_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let dot_buffer = make_instance_buffer(device, cells_as_bytes(dots));
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
        let ui_pass = unsafe { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = ui_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let ui_buffer = make_instance_buffer(device, ui_rects_as_bytes(ui_rects));
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
    let fg_pass = unsafe { MTLRenderPassDescriptor::new() };
    unsafe {
        let color = fg_pass.colorAttachments().objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setLoadAction(MTLLoadAction::Load);
        color.setStoreAction(MTLStoreAction::Store);
    }
    let fg_buffer = make_instance_buffer(device, glyphs_as_bytes(glyphs));
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
        let cfg_pass = unsafe { MTLRenderPassDescriptor::new() };
        unsafe {
            let color = cfg_pass.colorAttachments().objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Load);
            color.setStoreAction(MTLStoreAction::Store);
        }
        let cfg_buffer = make_instance_buffer(device, glyphs_as_bytes(color_glyphs));
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
}

/// Allocate a render-target MTLTexture.  Helper for tests + the
/// `--bench metal-render` harness.  StorageModePrivate (GPU-only)
/// because we never read the bytes back in the bench path; for
/// readback (existing offscreen render_cells_bg_offscreen / fg)
/// the caller still allocates Managed + blit-synchronizes.
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
fn build_instances(
    layout: &Layout,
    views: &[SessionView],
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    window_focused: bool,
    hover_chrome_btn: Option<u8>,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    dots: &mut Vec<CellInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    let cell_w = font.cell_w as f32;
    let cell_h = font.cell_h as f32;
    let ascent = font.ascent as f32;
    let (atlas_w, atlas_h) = atlas.dims();
    let atlas_w_f = atlas_w as f32;
    let atlas_h_f = atlas_h as f32;

    // Sidebar BG = cell BG (already covered by the clear pass).  No
    // explicit fill needed unless the sidebar palette ever diverges.

    // Every internal hairline — sidebar↔grid, header↔grid, and
    // cell↔cell — uses one uniformly weak SEAM tone at one width.
    // The eye reads structure (this is a sidebar / this is a grid /
    // this is a cell) without any seam taking on chrome weight.
    if layout.gutter > 0.0 && !layout.cells.is_empty() {
        let g = layout.gutter as f32;
        let inset = layout.top_inset as f32;
        let avail_h = (layout.window_h - layout.top_inset) as f32;
        let color = [SEAM.0, SEAM.1, SEAM.2, 1.0];
        // Sidebar↔grid vertical seam (in the strip the layout
        // reserved immediately after the sidebar).
        if layout.sidebar_w > 0.0 {
            cells.push(CellInstance {
                origin: [layout.sidebar_w as f32, inset],
                size: [g, avail_h],
                color,
            });
        }
        // Header↔grid horizontal seam — spans the full window width
        // above the 9-grid (right of sidebar+seam if a sidebar
        // exists; full width otherwise).  Sits exactly at y =
        // top_inset, so it visually closes the top of the grid the
        // same way the rounded window edge closes its sides.
        if layout.top_inset > 0.0 {
            cells.push(CellInstance {
                origin: [0.0, inset - g],
                size: [layout.window_w as f32, g],
                color,
            });
        }
        // Title-strip↔toolbar horizontal seam — same SEAM tone as
        // the header↔grid hairline above, just one band up.  Splits
        // the top chrome into the (L1-bound) version label area and
        // the (L2-bound) icon-button toolbar.  Width math mirrors
        // top_inset's split: title takes TITLE_STRIP_PT / HEADER_PT
        // of the total chrome.
        if layout.top_inset > 0.0 {
            let title_h = (layout.top_inset as f32)
                * (crate::TITLE_STRIP_PT / crate::HEADER_PT) as f32;
            if title_h > 0.0 {
                cells.push(CellInstance {
                    origin: [0.0, title_h - g],
                    size: [layout.window_w as f32, g],
                    color,
                });
            }
        }
        // Inter-cell vertical seams.
        for c in 1..layout.grid_cols {
            let prev = layout.cells[c - 1];
            cells.push(CellInstance {
                origin: [(prev.x + prev.w) as f32, inset],
                size: [g, avail_h],
                color,
            });
        }
        // Inter-cell horizontal seams.
        let grid_left = layout
            .cells
            .first()
            .map(|c| c.x as f32)
            .unwrap_or(layout.sidebar_w as f32);
        let grid_right = layout
            .cells
            .last()
            .map(|c| (c.x + c.w) as f32)
            .unwrap_or(layout.window_w as f32);
        let avail_w_grid = grid_right - grid_left;
        for r in 1..layout.grid_rows {
            let prev = layout.cells[(r - 1) * layout.grid_cols];
            cells.push(CellInstance {
                origin: [grid_left, (prev.y_top + prev.h) as f32],
                size: [avail_w_grid, g],
                color,
            });
        }
    }

    for (i, view) in views.iter().enumerate() {
        let rect = match layout.cells.get(i) {
            Some(r) => r,
            None => continue,
        };
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
    }

    // Cells past the last view are "empty" — N sessions < layout
    // cells.  Paint a subtle highlight + a centred [+] glyph so
    // they read as "open slot" rather than "broken layout".  The
    // [+] glyph itself goes in `push_empty_cell_glyphs` below
    // because it needs the atlas + font borrows.
    push_empty_cells(layout, views.len(), cells);

    if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
        push_sidebar(
            sidebar,
            focused_idx,
            layout.sidebar_w as f32,
            layout.top_inset as f32,
            layout.sidebar_top_pad_phys as f32,
            cell_w,
            cell_h,
            ascent,
            atlas_w_f,
            atlas_h_f,
            font,
            atlas,
            cells,
            glyphs,
            dots,
        );
    }

    // Floating chrome (layout button + picker overlay + close BGs
    // + add-button BG).  Drawn last so it composites over the
    // cells / sidebar.  Picker only paints when its rect is `Some`.
    push_layout_chrome(layout, hover_chrome_btn, cells);
    // Close-[×] and add-[+] glyphs piggy-back on the FG (atlas)
    // pipeline so they're real font glyphs (× = U+00D7, + = U+002B)
    // — not axis-aligned rect crosses.
    push_close_glyphs(
        layout,
        cell_w,
        cell_h,
        ascent,
        atlas_w_f,
        atlas_h_f,
        font,
        atlas,
        glyphs,
    );
    push_add_button_glyph(
        layout,
        cell_w,
        cell_h,
        ascent,
        atlas_w_f,
        atlas_h_f,
        font,
        atlas,
        glyphs,
    );
    // Empty-cell [+] hints — drawn through the FG pipeline because
    // the glyph wants real font shape, not a rect cross.
    push_empty_cell_glyphs(
        layout,
        views.len(),
        cell_w,
        cell_h,
        ascent,
        atlas_w_f,
        atlas_h_f,
        font,
        atlas,
        glyphs,
    );

    // Header version label — quiet metadata in the header strip,
    // right-aligned just left of the chrome buttons (or the window
    // edge when there are none).  The git sha is stamped per build,
    // so this string changes on every silent update — the user sees
    // the new core land here.
    if layout.top_inset > 0.0 {
        let label = version_label();
        let text_w = label.chars().count() as f32 * cell_w;
        // Version label lives in the *title strip* (top portion of
        // top_inset); the toolbar below holds the icon buttons.
        // Right-align with a small margin so the label hugs the
        // window edge, not floats relative to button position.
        let title_h = (layout.top_inset as f32)
            * (crate::TITLE_STRIP_PT / crate::HEADER_PT) as f32;
        let right_margin_logical_pt: f32 = 8.0;
        let scale_approx = (layout.top_inset as f32) / crate::HEADER_PT as f32;
        let right_margin_phys = right_margin_logical_pt * scale_approx;
        let x =
            ((layout.window_w as f32) - right_margin_phys - text_w).max(cell_w);
        let baseline_y =
            ((title_h - cell_h) * 0.5).max(0.0) + ascent;
        push_text_run(
            &label,
            x,
            baseline_y,
            [HEADER_VERSION_FG.0, HEADER_VERSION_FG.1, HEADER_VERSION_FG.2, 1.0],
            cell_w,
            cell_h,
            ascent,
            atlas_w_f,
            atlas_h_f,
            font,
            atlas,
            glyphs,
        );
    }
}

/// Empty-cell BG tint.  Painted over `layout.cells[views.len()..]`
/// when sessions count is less than layout cell count so the
/// "open slot" reads as a softer, slightly elevated area.  Low-
/// alpha black-ish overlay over the existing cell BG keeps the
/// terminal palette intact.
const EMPTY_CELL_OVERLAY: [f32; 4] = [0.04, 0.05, 0.07, 0.6];
const EMPTY_CELL_GLYPH_FG: [f32; 4] = [0.30, 0.34, 0.38, 0.85];

fn push_empty_cells(
    layout: &Layout,
    n_visible: usize,
    cells: &mut Vec<CellInstance>,
) {
    for rect in layout.cells.iter().skip(n_visible) {
        // Skip the cell title strip area so empties read with the
        // same band as live sessions (no title strip painted).
        let inner_top = rect.y_top + layout.cell_title_h;
        let inner_h = (rect.h - layout.cell_title_h).max(0.0);
        push_rect(
            cells,
            Rect {
                x: rect.x,
                y_top: inner_top,
                w: rect.w,
                h: inner_h,
            },
            EMPTY_CELL_OVERLAY,
        );
    }
}

/// Centre a `+` glyph in each empty cell — a lightweight hint
/// that the slot exists and (eventually) accepts a click to
/// spawn.  Today empty-cell clicks are a no-op; the user spawns
/// via the sidebar `[+]`.
#[allow(clippy::too_many_arguments)]
fn push_empty_cell_glyphs(
    layout: &Layout,
    n_visible: usize,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    if layout.cells.len() <= n_visible {
        return;
    }
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let (font_idx, glyph) = font.resolve_char('+', false, false);
    if glyph == 0 {
        return;
    }
    let ct_font = font.font(font_idx).clone();
    let entry = match atlas.get_or_rasterize(
        GlyphKey {
            font_id: font_idx as u32,
            glyph,
        },
        &ct_font,
        metrics,
        1,
    ) {
        Some(e) => e,
        None => return,
    };
    let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
    let slot_h = metrics.cell_h as f32;
    for rect in layout.cells.iter().skip(n_visible) {
        let inner_top = rect.y_top + layout.cell_title_h;
        let inner_h = (rect.h - layout.cell_title_h).max(0.0);
        let cx = rect.x as f32 + rect.w as f32 / 2.0;
        let cy = (inner_top + inner_h / 2.0) as f32;
        glyphs.push(GlyphInstance {
            origin: [(cx - slot_w / 2.0).round(), (cy - slot_h / 2.0).round()],
            size: [slot_w, slot_h],
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color: EMPTY_CELL_GLYPH_FG,
        });
    }
}

/// Same FG-pipeline trick as `push_close_glyphs`, but for the
/// sidebar [+] add-session button.  Glyph is `+` (U+002B PLUS
/// SIGN); colour follows the disabled / enabled state derived
/// from `close_session_rects.len()` (== n_sessions).
#[allow(clippy::too_many_arguments)]
fn push_add_button_glyph(
    layout: &Layout,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let rect = layout.add_session_button_rect;
    if rect.w <= 0.0 {
        return;
    }
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let (font_idx, glyph) = font.resolve_char('+', false, false);
    if glyph == 0 {
        return;
    }
    let ct_font = font.font(font_idx).clone();
    let entry = match atlas.get_or_rasterize(
        GlyphKey {
            font_id: font_idx as u32,
            glyph,
        },
        &ct_font,
        metrics,
        1,
    ) {
        Some(e) => e,
        None => return,
    };
    let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
    let slot_h = metrics.cell_h as f32;
    let n_sessions = layout.close_session_rects.len();
    let disabled = n_sessions >= SESSION_COUNT_HARD_CAP;
    let color = if disabled { ADD_BTN_FG_DISABLED } else { ADD_BTN_FG };
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y_top as f32 + rect.h as f32 / 2.0;
    glyphs.push(GlyphInstance {
        origin: [(cx - slot_w / 2.0).round(), (cy - slot_h / 2.0).round()],
        size: [slot_w, slot_h],
        uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
        uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
        color,
    });
}

// Chrome colours for the [layout] button and the picker overlay.
// Tuned against the cell BG (see font_cache::BG ≈ (0.006, 0.008,
// 0.014)) so the chrome reads as a slightly lighter raised surface,
// not as a competing pure-black panel.
const CHROME_BTN_BG: [f32; 4] = [0.085, 0.095, 0.115, 1.0];
const CHROME_BTN_BORDER: [f32; 4] = [0.18, 0.20, 0.23, 1.0];
const CHROME_PANEL_BG: [f32; 4] = [0.055, 0.065, 0.085, 1.0];
const CHROME_OPTION_BG: [f32; 4] = [0.13, 0.14, 0.17, 1.0];
const CHROME_ICON_FG: [f32; 4] = [0.55, 0.60, 0.65, 1.0];

fn push_rect(cells: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    cells.push(CellInstance {
        origin: [rect.x as f32, rect.y_top as f32],
        size: [rect.w as f32, rect.h as f32],
        color,
    });
}

/// Draw a 1-px-equivalent border around `rect` (four hairline rects)
/// in `color`.  Cheap — four extra instances; chrome only fires once
/// per frame.
fn push_border(
    cells: &mut Vec<CellInstance>,
    rect: Rect,
    width: f64,
    color: [f32; 4],
) {
    let w = width.max(1.0);
    // top
    push_rect(
        cells,
        Rect { x: rect.x, y_top: rect.y_top, w: rect.w, h: w },
        color,
    );
    // bottom
    push_rect(
        cells,
        Rect {
            x: rect.x,
            y_top: rect.y_top + rect.h - w,
            w: rect.w,
            h: w,
        },
        color,
    );
    // left
    push_rect(
        cells,
        Rect { x: rect.x, y_top: rect.y_top, w, h: rect.h },
        color,
    );
    // right
    push_rect(
        cells,
        Rect {
            x: rect.x + rect.w - w,
            y_top: rect.y_top,
            w,
            h: rect.h,
        },
        color,
    );
}

/// Draw a `dims.0 × dims.1` mini-grid of small filled rectangles
/// inside `container`.  Used for both the [layout] button (showing
/// the current grid shape) and each picker option (showing the
/// shape that option would switch to).  Pad shrinks the grid into
/// the container so a rim of CHROME_BTN_BG / CHROME_OPTION_BG shows
/// around it.
/// Lucide-style stroke width relative to the icon's inner box.  At a
/// 22pt button on 2× retina we get a ~30-px inner box, so 2 px feels
/// right — matches Lucide's 24px / stroke-2 default ratio.
fn icon_stroke(inner: Rect) -> f64 {
    (inner.w.min(inner.h) * 0.07).round().max(1.0)
}

/// Lucide `layout-grid` style: outer outline + interior dividers
/// drawn as thin lines (NOT filled cells).  `dims = (cols, rows)`
/// drives the divider count so the icon doubles as a "current grid
/// shape" indicator.
fn push_grid_icon(
    cells: &mut Vec<CellInstance>,
    container: Rect,
    dims: (usize, usize),
    color: [f32; 4],
) {
    let (gc, gr) = dims;
    if gc == 0 || gr == 0 {
        return;
    }
    let pad = (container.w.min(container.h) * 0.22).max(2.0);
    let inner_x = container.x + pad;
    let inner_y = container.y_top + pad;
    let inner_w = (container.w - 2.0 * pad).max(1.0);
    let inner_h = (container.h - 2.0 * pad).max(1.0);
    let frame = Rect {
        x: inner_x,
        y_top: inner_y,
        w: inner_w,
        h: inner_h,
    };
    let stroke = icon_stroke(frame);
    push_border(cells, frame, stroke, color);
    // Interior column dividers — (gc - 1) thin vertical strokes
    // evenly distributed across the inner width.
    for c in 1..gc {
        let x = inner_x + c as f64 * inner_w / gc as f64 - stroke * 0.5;
        push_rect(
            cells,
            Rect { x, y_top: inner_y, w: stroke, h: inner_h },
            color,
        );
    }
    // Interior row dividers — (gr - 1) thin horizontal strokes.
    for r in 1..gr {
        let y = inner_y + r as f64 * inner_h / gr as f64 - stroke * 0.5;
        push_rect(
            cells,
            Rect { x: inner_x, y_top: y, w: inner_w, h: stroke },
            color,
        );
    }
}

/// Lucide `panel-left` style: outer outline + a single vertical
/// divider at 1/3 of the inner width.  When `collapsed`, the divider
/// + the would-be-panel region dims so the icon reads as a state
/// indicator ("sidebar showing" vs "sidebar hidden") at a glance.
fn push_sidebar_icon(
    cells: &mut Vec<CellInstance>,
    container: Rect,
    collapsed: bool,
) {
    let pad = (container.w.min(container.h) * 0.22).max(2.0);
    let inner_x = container.x + pad;
    let inner_y = container.y_top + pad;
    let inner_w = (container.w - 2.0 * pad).max(1.0);
    let inner_h = (container.h - 2.0 * pad).max(1.0);
    let frame = Rect {
        x: inner_x,
        y_top: inner_y,
        w: inner_w,
        h: inner_h,
    };
    let stroke = icon_stroke(frame);
    let frame_color = if collapsed {
        [
            CHROME_ICON_FG[0] * 0.55,
            CHROME_ICON_FG[1] * 0.55,
            CHROME_ICON_FG[2] * 0.55,
            CHROME_ICON_FG[3],
        ]
    } else {
        CHROME_ICON_FG
    };
    push_border(cells, frame, stroke, frame_color);
    // Vertical divider at ~1/3 of the inner width — same stroke as
    // the frame so the icon reads as a single line drawing.  The
    // divider stays at the brighter colour even when collapsed so
    // "the affordance toggles sidebar visibility" still reads.
    let div_x = inner_x + (inner_w / 3.0).round() - stroke * 0.5;
    push_rect(
        cells,
        Rect { x: div_x, y_top: inner_y, w: stroke, h: inner_h },
        CHROME_ICON_FG,
    );
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
// "constructive action".  Disabled (N == 9) drops to gray —
// `mouse_down` ignores the click but the dim look explains why.
const ADD_BTN_BG: [f32; 4] = [0.07, 0.16, 0.10, 0.85];
const ADD_BTN_FG: [f32; 4] = [0.65, 0.92, 0.72, 1.0];
const ADD_BTN_BG_DISABLED: [f32; 4] = [0.10, 0.10, 0.11, 0.65];
const ADD_BTN_FG_DISABLED: [f32; 4] = [0.45, 0.45, 0.47, 0.8];

const SESSION_COUNT_HARD_CAP: usize = 9;

/// BG fill used when the cursor is over an icon button.  Slightly
/// darker than CHROME_BTN_BG so the hovered button "presses in" —
/// matches modern minimal-icon-button affordances (Lucide / shadcn
/// idiom: idle = barely-visible BG, hover = a few % deeper).
const CHROME_BTN_BG_HOVER: [f32; 4] = [0.130, 0.140, 0.165, 1.0];

fn push_layout_chrome(
    layout: &Layout,
    hover_chrome_btn: Option<u8>,
    cells: &mut Vec<CellInstance>,
) {
    // Per-button BG: darker fill under cursor.  Cursor hover state
    // is sent by L2 on every MouseMove and ignored when None
    // (window unfocused or mouse outside both buttons).
    let sidebar_bg = if hover_chrome_btn == Some(0) {
        CHROME_BTN_BG_HOVER
    } else {
        CHROME_BTN_BG
    };
    let layout_bg = if hover_chrome_btn == Some(1) {
        CHROME_BTN_BG_HOVER
    } else {
        CHROME_BTN_BG
    };
    // Sidebar toggle button — sits left of the layout button so the
    // user always has a way back when the sidebar is collapsed.  The
    // icon's "sidebar bar" dims when collapsed (state derived from
    // sidebar_w, kept in sync by `rebuild_layout_at`) so the
    // affordance doubles as a state indicator.
    let sidebar_collapsed = layout.sidebar_w == 0.0;
    push_rect(cells, layout.sidebar_button_rect, sidebar_bg);
    push_border(cells, layout.sidebar_button_rect, 1.0, CHROME_BTN_BORDER);
    push_sidebar_icon(
        cells,
        layout.sidebar_button_rect,
        sidebar_collapsed,
    );

    // Layout button is always present (even at 1×1); shows the
    // current grid shape so the user can tell at a glance.
    push_rect(cells, layout.layout_button_rect, layout_bg);
    push_border(cells, layout.layout_button_rect, 1.0, CHROME_BTN_BORDER);
    push_grid_icon(
        cells,
        layout.layout_button_rect,
        (layout.grid_cols, layout.grid_rows),
        CHROME_ICON_FG,
    );

    // Picker overlay.  Painted only when open; option rects are
    // pre-computed in `Layout::with_chrome`.
    if let Some(panel) = layout.picker_panel_rect {
        push_rect(cells, panel, CHROME_PANEL_BG);
        push_border(cells, panel, 1.0, CHROME_BTN_BORDER);
        for (rect, dims) in layout
            .picker_option_rects
            .iter()
            .zip(layout.picker_option_dims.iter())
        {
            push_rect(cells, *rect, CHROME_OPTION_BG);
            push_grid_icon(cells, *rect, *dims, CHROME_ICON_FG);
        }
    }

    // Sidebar close-[×] BG tints (FG `×` glyph is laid down later
    // in `push_close_glyphs`, since glyphs need atlas + font).
    // When this is the last remaining session, the BG fades to
    // gray — `mouse_down` already refuses the click, the dim look
    // tells the user *why* nothing happened.
    let n_sessions = layout.close_session_rects.len();
    let close_disabled = n_sessions == 1;
    let close_bg = if close_disabled {
        CLOSE_BTN_BG_DISABLED
    } else {
        CLOSE_BTN_BG
    };
    for rect in &layout.close_session_rects {
        push_rect(cells, *rect, close_bg);
    }

    // Sidebar [+] add-session button BG (FG `+` glyph laid down
    // later in `push_add_button_glyph`).  Disabled at the hard cap
    // of 9 sessions; `mouse_down` ignores the click then.  Painted
    // before close × so close × always reads as a per-row
    // affordance even on the same y-band.
    if layout.add_session_button_rect.w > 0.0 {
        let add_disabled = n_sessions >= SESSION_COUNT_HARD_CAP;
        let add_bg = if add_disabled {
            ADD_BTN_BG_DISABLED
        } else {
            ADD_BTN_BG
        };
        push_rect(cells, layout.add_session_button_rect, add_bg);
    }
}

/// Rasterise the `×` glyph (Unicode U+00D7 MULTIPLICATION SIGN)
/// once via the existing atlas + font pipeline, then push one
/// `GlyphInstance` per close button so the FG pass paints a real
/// diagonal cross — not the axis-aligned `+` shape a hand-drawn
/// rect-pair would produce.  Slot is cell-sized; centred over each
/// button (button is smaller than the cell, but the actual `×` ink
/// sits in the slot's middle and lands inside the button BG).
#[allow(clippy::too_many_arguments)]
fn push_close_glyphs(
    layout: &Layout,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    if layout.close_session_rects.is_empty() {
        return;
    }
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let (font_idx, glyph) = font.resolve_char('×', false, false);
    if glyph == 0 {
        return;
    }
    let ct_font = font.font(font_idx).clone();
    let entry = match atlas.get_or_rasterize(
        GlyphKey {
            font_id: font_idx as u32,
            glyph,
        },
        &ct_font,
        metrics,
        1,
    ) {
        Some(e) => e,
        None => return,
    };
    let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
    let slot_h = metrics.cell_h as f32;
    let disabled = layout.close_session_rects.len() == 1;
    let color = if disabled { CLOSE_BTN_FG_DISABLED } else { CLOSE_BTN_FG };
    for rect in &layout.close_session_rects {
        let cx = rect.x as f32 + rect.w as f32 / 2.0;
        let cy = rect.y_top as f32 + rect.h as f32 / 2.0;
        glyphs.push(GlyphInstance {
            origin: [(cx - slot_w / 2.0).round(), (cy - slot_h / 2.0).round()],
            size: [slot_w, slot_h],
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color,
        });
    }
}

/// Sidebar rows.  Per row: optional focus-bg highlight, a state
/// dot (rendered through the dot pipeline so it's a real circle,
/// not a square), and the label glyphs.  Mirrors
/// `Renderer::draw_sidebar` in `render.rs` so click hit-testing on
/// the same constants lands on the same pixels.
#[allow(clippy::too_many_arguments)]
fn push_sidebar(
    entries: &[SidebarEntry],
    focused_idx: usize,
    sidebar_w: f32,
    top_inset: f32,
    sidebar_top_pad: f32,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    dots: &mut Vec<CellInstance>,
) {
    for (i, entry) in entries.iter().enumerate() {
        let row_top_y = top_inset + sidebar_top_pad + i as f32 * SIDEBAR_ROW_H;

        if i == focused_idx {
            cells.push(CellInstance {
                origin: [0.0, row_top_y],
                size: [sidebar_w, SIDEBAR_ROW_H],
                color: [
                    BG_FOCUSED.0,
                    BG_FOCUSED.1,
                    BG_FOCUSED.2,
                    1.0,
                ],
            });
        }

        let dot_color = match entry.state {
            SessionState::Active => STATE_ACTIVE,
            SessionState::Idle => STATE_IDLE,
            SessionState::Exited => STATE_EXITED,
        };
        let dot_cx = SIDEBAR_LEFT_PAD + SIDEBAR_DOT_R;
        let dot_cy = row_top_y + SIDEBAR_ROW_H / 2.0;
        // Routed through the dot pipeline (separate vec): same quad,
        // but its fragment shader smoothsteps to a circle.
        dots.push(CellInstance {
            origin: [dot_cx - SIDEBAR_DOT_R, dot_cy - SIDEBAR_DOT_R],
            size: [SIDEBAR_DOT_R * 2.0, SIDEBAR_DOT_R * 2.0],
            color: [dot_color.0, dot_color.1, dot_color.2, 1.0],
        });

        // Label text.  Lay out monospace via cell_w (sidebar labels
        // are ASCII / short tmux names, so cell_w accuracy is fine).
        let label_x = dot_cx + SIDEBAR_DOT_R + SIDEBAR_DOT_LABEL_GAP;
        // Align the visual centre of an ASCII digit / letter with the
        // dot's centre.  cap_height ≈ 0.65 × ascent for most monospace
        // faces, so half of cap_height ≈ 0.30 × ascent below the
        // baseline (in y-down).  The previous formula
        // `dot_cy + ascent*0.40 - SIDEBAR_ROW_H*0.20` happened to land
        // okay on Menlo 13 but pushes Monaco 12 digits ~3 px above the
        // dot — mismatch the user reported as "menulist 没对齐".
        let metrics = SlotMetrics {
            cell_w: cell_w.round() as u32,
            cell_h: cell_h.round() as u32,
            baseline_from_top: ascent.round() as u32,
        };
        // Where the digit's BASELINE should land on screen.  Aligns the
        // visual centre of an ASCII digit with the dot's centre:
        // cap_height ≈ 0.65 × ascent, so half of cap-height ≈ 0.30 ×
        // ascent below dot_cy (y-down).
        let baseline_y = dot_cy + ascent * 0.30;
        // Slot top = baseline minus baseline_from_top.
        let slot_top_y = baseline_y - metrics.baseline_from_top as f32;
        let mut x = label_x;
        for ch in entry.label.chars() {
            let (font_idx, glyph) = font.resolve_char(ch, false, false);
            if glyph != 0 {
                let ct_font = font.font(font_idx).clone();
                let n_cells = crate::grid::char_width(ch).max(1) as u16;
                if let Some(e) = atlas.get_or_rasterize(
                    GlyphKey {
                        font_id: font_idx as u32,
                        glyph,
                    },
                    &ct_font,
                    metrics,
                    n_cells,
                ) {
                    let slot_w = (metrics.cell_w * e.n_cells as u32) as f32;
                    glyphs.push(GlyphInstance {
                        origin: [x.round(), slot_top_y.round()],
                        size: [slot_w, metrics.cell_h as f32],
                        uv0: [e.u0 as f32 / atlas_w, e.v0 as f32 / atlas_h],
                        uv1: [e.u1 as f32 / atlas_w, e.v1 as f32 / atlas_h],
                        color: [
                            SIDEBAR_TEXT_FG.0,
                            SIDEBAR_TEXT_FG.1,
                            SIDEBAR_TEXT_FG.2,
                            1.0,
                        ],
                    });
                }
            }
            x += cell_w;
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// The running binary's version label, e.g. `Marspot v0.2.0 (1a7b23c5)`.
/// `MARSPOT_GIT_SHA` is stamped per build (build.rs), so this string
/// changes on every silent update — the header renders it as visible
/// proof the new core landed.
pub fn version_label() -> String {
    // L2 (marspot-core) is the canonical marspot version — the title
    // bar shows it, since the renderer is what users actually interact
    // with. The other layers in the four-layer split (L1 shell, L3
    // session, L4 shelld) carry their own semver in version-vector.toml
    // for operator diagnostics, but the headline number is L2's.
    format!(
        "Marspot v{} ({})",
        env!("MARSPOT_VERSION_CORE"),
        env!("MARSPOT_GIT_SHA"),
    )
}

/// Lay a run of text starting at baseline `(x, baseline_y)` in
/// physical pixels, advancing one monospace cell per char.  Shared
/// by the cell-title strip and the header version label.
#[allow(clippy::too_many_arguments)]
fn push_text_run(
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
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let mut x = x_start;
    for ch in text.chars() {
        let (font_idx, glyph) = font.resolve_char(ch, false, false);
        if glyph != 0 {
            let ct_font = font.font(font_idx).clone();
            let n_cells = crate::grid::char_width(ch).max(1) as u16;
            if let Some(entry) = atlas.get_or_rasterize(
                GlyphKey { font_id: font_idx as u32, glyph },
                &ct_font,
                metrics,
                n_cells,
            ) {
                let dest_y = (baseline_y - ascent).round();
                let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
                glyphs.push(GlyphInstance {
                    origin: [x.round(), dest_y],
                    size: [slot_w, metrics.cell_h as f32],
                    uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                    uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                    color,
                });
            }
        }
        x += cell_w;
    }
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
    ui_rects: &mut Vec<UiRectInstance>,
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
    let under_overlay = |row: u16, col: u16| -> bool {
        match overlay_mask {
            Some((r0, r1, c0, c1)) => row >= r0 && row < r1 && col >= c0 && col <= c1,
            None => false,
        }
    };

    // Scan once up front so the per-row glyph loop can override fg
    // for cells inside a link span (paint the text the same cyan as
    // the underline, the standard "this is clickable" cue) and the
    // underline pass below can reuse the same list.
    // cc-mode: a non-empty plugin badge identifies a claudecode pane;
    // tell the link scanner so it merges the hanging-indent
    // continuation rows into one logical URL/path token.  Inert on
    // non-cc panes (badge is empty).  See `ScanOpts::cc_mode`.
    let link_opts = marspot_term::grid_links::ScanOpts {
        cc_mode: !view.right_badge.is_empty(),
    };
    let links = marspot_term::grid_links::scan_visible_links(grid, view.view_offset, link_opts);
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
            // Cell-sized slot: place the WHOLE slot at the cell
            // origin.  The glyph's baseline is at integer row
            // `baseline_from_top` inside the slot, identical for
            // every glyph at this font/size — so every glyph's
            // baseline lands on screen row `row_y + baseline_from_top`
            // exactly.  No bearing maths, no fractional dest_y, no
            // sub-pixel inter-glyph drift.
            let dest_x = (inner_x + c as f32 * cell_w).round();
            let dest_y = row_y.round();
            let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
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
                origin: [dest_x, dest_y],
                size: [slot_w, metrics.cell_h as f32],
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
                // Cell-sized slot — place at cell origin (see
                // comment in main glyph push).
                let dest_x = (inner_x + col as f32 * cell_w).round();
                let dest_y = (inner_y + (row as f32) * cell_h).round();
                let slot_w = (metrics.cell_w * entry.n_cells as u32) as f32;
                glyphs.push(GlyphInstance {
                    origin: [dest_x, dest_y],
                    size: [slot_w, metrics.cell_h as f32],
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
                    glyphs.push(GlyphInstance {
                        origin: [dest_x, dest_y],
                        size: [
                            (metrics.cell_w * entry.n_cells as u32) as f32,
                            metrics.cell_h as f32,
                        ],
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
    if let Some(overlay) = view.search_overlay.as_ref() {
        const OVERLAY_COLS: u16 = SEARCH_BAR_COLS;
        const LIST_MAX_ROWS: u16 = SEARCH_LIST_MAX_ROWS;
        const OVERLAY_BG: (f32, f32, f32, f32) = (0.13, 0.14, 0.17, 1.0);
        const OVERLAY_BORDER: (f32, f32, f32, f32) = (0.30, 0.32, 0.38, 1.0);
        const OVERLAY_TEXT: (f32, f32, f32) = (0.95, 0.96, 0.97);
        const OVERLAY_DIM: (f32, f32, f32) = (0.60, 0.63, 0.70);
        const OVERLAY_ACCENT: (f32, f32, f32) = (0.40, 0.62, 1.0);
        const OVERLAY_FOCUSED_BG: (f32, f32, f32, f32) = (0.18, 0.28, 0.48, 1.0);
        const OVERLAY_DIVIDER: (f32, f32, f32, f32) = (0.22, 0.24, 0.28, 1.0);
        const PANEL_RADIUS_PX: f32 = 10.0;
        const ROW_RADIUS_PX: f32 = 5.0;
        const SHADOW_BLUR_PX: f32 = 18.0;
        const SHADOW_ALPHA: f32 = 0.45;
        let cols = grid.cols();
        let rows = grid.rows();
        if cols >= OVERLAY_COLS + 2 {
            let n_list = (overlay.hits.len() as u16).min(LIST_MAX_ROWS);
            let has_list = n_list > 0;
            // Visual rows occupied (text rows): query (1) + divider gap (½) +
            // list rows.  Total panel height is approximated in cells so
            // it scales with font, then padded for breathing room.
            let chrome_rows: f32 = 1.0; // query row
            let divider_rows: f32 = if has_list { 0.4 } else { 0.0 };
            let list_rows: f32 = if has_list { n_list as f32 } else { 0.0 };
            let total_rows_f = chrome_rows + divider_rows + list_rows;
            // Panel height: total text rows + outer padding (½ row top + ½ bottom).
            let panel_inner_pad = (cell_h * 0.4).max(6.0);
            let panel_h = (total_rows_f * cell_h).round() + 2.0 * panel_inner_pad;
            // Clamp height to grid bottom so the overlay never overshoots.
            let grid_bottom = inner_y + rows as f32 * cell_h;

            let bar_left_col = cols - OVERLAY_COLS - 1;
            let bar_x = inner_x + bar_left_col as f32 * cell_w;
            // Anchor: small floating margin from grid top (looks like
            // a popover, not flush-attached chrome).
            let panel_y = inner_y + (cell_h * 0.5).round();
            let panel_w = OVERLAY_COLS as f32 * cell_w;
            let panel_h = panel_h.min((grid_bottom - panel_y).max(0.0));

            // 1. Panel: rounded rect + 1px stroke + drop shadow, all
            //    computed in one instance via the SDF shader.
            ui_rects.push(UiRectInstance {
                origin: [bar_x, panel_y],
                size: [panel_w, panel_h],
                fill_color: [OVERLAY_BG.0, OVERLAY_BG.1, OVERLAY_BG.2, OVERLAY_BG.3],
                border_color: [
                    OVERLAY_BORDER.0,
                    OVERLAY_BORDER.1,
                    OVERLAY_BORDER.2,
                    OVERLAY_BORDER.3,
                ],
                corner_radius: PANEL_RADIUS_PX,
                border_width: 1.0,
                shadow_blur: SHADOW_BLUR_PX,
                shadow_alpha: SHADOW_ALPHA,
                shadow_color: [0.0, 0.0, 0.0, 1.0],
            });

            // Inner content origin (after panel padding).
            let inner_left = bar_x + panel_inner_pad;
            let inner_top = panel_y + panel_inner_pad;

            let text_color = [OVERLAY_TEXT.0, OVERLAY_TEXT.1, OVERLAY_TEXT.2, 1.0];
            let dim_color = [OVERLAY_DIM.0, OVERLAY_DIM.1, OVERLAY_DIM.2, 1.0];
            let accent_color = [OVERLAY_ACCENT.0, OVERLAY_ACCENT.1, OVERLAY_ACCENT.2, 1.0];

            // 2. Query row: text + caret + right-aligned counter +
            //    Aa toggle + × close.  Pixel-positioned, not
            //    cell-aligned.
            let query_baseline = inner_top + ascent;
            let query_x = inner_left;
            let query_max_chars = (OVERLAY_COLS - 12) as usize;
            let query_display: String =
                overlay.query.chars().take(query_max_chars).collect();
            if !query_display.is_empty() {
                push_text_run(
                    &query_display, query_x, query_baseline, text_color,
                    cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, glyphs,
                );
            }
            // Caret — thin accent stripe at the query_cursor column.
            let caret_col = (overlay.query_cursor as usize).min(query_max_chars) as f32;
            let caret_x = query_x + caret_col * cell_w;
            cells.push(CellInstance {
                origin: [caret_x, inner_top + 2.0],
                size: [2.0, cell_h - 4.0],
                color: [OVERLAY_ACCENT.0, OVERLAY_ACCENT.1, OVERLAY_ACCENT.2, 0.95],
            });

            // Right side: × close hint, Aa toggle, counter "4/64".
            let close_x = bar_x + panel_w - panel_inner_pad - cell_w;
            push_text_run(
                "×", close_x, query_baseline, dim_color,
                cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, glyphs,
            );
            let aa_x = close_x - 3.0 * cell_w;
            let aa_color = if overlay.case_sensitive { accent_color } else { dim_color };
            push_text_run(
                "Aa", aa_x, query_baseline, aa_color,
                cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, glyphs,
            );
            if let Some((c, t)) = overlay.counter {
                let counter_text = format!("{c}/{t}");
                let counter_w_chars = counter_text.chars().count() as f32;
                let counter_x = aa_x - (counter_w_chars + 1.0) * cell_w;
                push_text_run(
                    &counter_text, counter_x, query_baseline, dim_color,
                    cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, glyphs,
                );
            }

            // 3. Divider — 1px hairline rounded rect across the panel
            //    when a list is shown.
            let divider_y = inner_top + cell_h + (panel_inner_pad * 0.5).round();
            if has_list {
                ui_rects.push(UiRectInstance {
                    origin: [inner_left, divider_y],
                    size: [panel_w - 2.0 * panel_inner_pad, 1.0],
                    fill_color: [
                        OVERLAY_DIVIDER.0,
                        OVERLAY_DIVIDER.1,
                        OVERLAY_DIVIDER.2,
                        OVERLAY_DIVIDER.3,
                    ],
                    border_color: [0.0, 0.0, 0.0, 0.0],
                    corner_radius: 0.5,
                    border_width: 0.0,
                    shadow_blur: 0.0,
                    shadow_alpha: 0.0,
                    shadow_color: [0.0, 0.0, 0.0, 0.0],
                });
            }

            // 4. List rows.
            let list_top = divider_y + (panel_inner_pad * 0.5).round();
            let max_visible = ((grid_bottom - list_top) / cell_h).floor() as u16;
            let visible = n_list.min(max_visible).min(LIST_MAX_ROWS);
            for i in 0..visible {
                let h = &overlay.hits[i as usize];
                let row_y_px = list_top + i as f32 * cell_h;
                let row_baseline_px = row_y_px + ascent;
                if h.is_focused {
                    // Rounded indigo selection bar, inset 4px from
                    // the panel edges so the corner radius reads.
                    let inset = 4.0;
                    ui_rects.push(UiRectInstance {
                        origin: [bar_x + inset, row_y_px],
                        size: [panel_w - 2.0 * inset, cell_h],
                        fill_color: [
                            OVERLAY_FOCUSED_BG.0,
                            OVERLAY_FOCUSED_BG.1,
                            OVERLAY_FOCUSED_BG.2,
                            OVERLAY_FOCUSED_BG.3,
                        ],
                        border_color: [0.0, 0.0, 0.0, 0.0],
                        corner_radius: ROW_RADIUS_PX,
                        border_width: 0.0,
                        shadow_blur: 0.0,
                        shadow_alpha: 0.0,
                        shadow_color: [0.0, 0.0, 0.0, 0.0],
                    });
                }
                let inner_w_chars = (OVERLAY_COLS - 2) as usize;
                let snip: String = h.snippet.chars().take(inner_w_chars).collect();
                let snip_color = if h.is_focused { text_color } else { dim_color };
                push_text_run(
                    &snip, inner_left, row_baseline_px, snip_color,
                    cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, glyphs,
                );
            }
        }
        let _ = rows;
    }
}

/// Build a Shared-storage MTLBuffer over `bytes`.  Returns `None`
/// for an empty payload so the caller can skip the bind/draw.
fn make_instance_buffer(
    device: &ProtocolObject<dyn MTLDevice>,
    bytes: &[u8],
) -> Option<Retained<ProtocolObject<dyn objc2_metal::MTLBuffer>>> {
    if bytes.is_empty() {
        return None;
    }
    unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(),
            bytes.len(),
            MTLResourceOptions::MTLResourceStorageModeShared,
        )
    }
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
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
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
                    MTLResourceOptions::MTLResourceStorageModeShared,
                )
            }
        };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
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
        unsafe { cmd.waitUntilCompleted() };

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
                    MTLResourceOptions::MTLResourceStorageModeShared,
                )
            }
        };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
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
        unsafe { cmd.waitUntilCompleted() };

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
    // SAFETY: MTLCreateSystemDefaultDevice returns a +1 retained pointer
    // (per Apple docs) or null on failure.  Wrap with Retained::from_raw
    // to take ownership without an extra retain.
    let raw = unsafe { MTLCreateSystemDefaultDevice() };
    if raw.is_null() {
        return Err("MTLCreateSystemDefaultDevice returned nil — no Metal device".into());
    }
    unsafe { Retained::from_raw(raw) }
        .ok_or_else(|| "MTLCreateSystemDefaultDevice → null after non-null check".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use crate::glyph_atlas::{GlyphAtlas, GlyphKey};
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
                GlyphKey { font_id: 0, glyph: cg_glyph },
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

    /// Translation-layer smoke test: feed a tiny one-session layout
    /// + a grid with "AB" on it through `build_instances` and check
    /// that the scratch vecs come out populated.  Doesn't render —
    /// the BG/FG passes are tested end-to-end in the offscreen
    /// tests above.
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
            right_badge: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
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
            right_badge: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
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
            right_badge: "",
            top_fixed_h_cells: top_fixed,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
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
        use crate::grid::{Cell, Grid};
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
            right_badge: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
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
            right_badge: view.right_badge,
            top_fixed_h_cells: view.top_fixed_h_cells,
            bot_fixed_h_cells: view.bot_fixed_h_cells,
            highlight_spans: &[],
            search_overlay: None,
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
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
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
            right_badge: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
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
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
        };
        let view_bot = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", update_pending: false,
            right_badge: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 2,
            highlight_spans: &[],
            search_overlay: None,
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
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
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
}
