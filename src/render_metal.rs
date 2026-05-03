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
//!    mars without disturbing the AppKit renderer.
//! 2. **Glyph atlas** — CoreText-rasterise glyphs into an MTLTexture,
//!    LRU-evict per CLAUDE.md "bounded growth".
//! 3. **BG pass** — instanced coloured quads, one per cell.
//! 4. **FG pass** — textured glyph quads sampling the atlas.
//! 5. **Integration** — wire to the terminal grid; A/B against
//!    `Renderer` via `MARS_METAL=1`.  Once visually equivalent and
//!    measurably faster, retire the AppKit path.
//!
//! ## Why "previously failed at this" doesn't apply
//!
//! `render.rs`'s header notes mars *did* try Metal+atlas before and
//! pivoted away due to "gamma + atlas neighbor + sampling issues".
//! This rebuild is informed by that — explicit gamma in the shader
//! (sRGB pixel format, premultiplied alpha), atlas allocator that
//! pads each glyph by 1 px (no neighbour bleed), and exact-pixel
//! sampling with `MTLSamplerMinMagFilter::Nearest` for the BG pass
//! and `Linear` only for the glyph pass.  Full design notes go into
//! `docs/architecture.md` once phase 4 lands.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::{CGSize, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLSamplerAddressMode, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState, MTLStoreAction, MTLTexture,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;
use std::ptr::NonNull;

use crate::font_cache::{resolve_attrs, FontCache, BG};
use crate::glyph_atlas::{GlyphAtlas, GlyphKey};
use crate::grid::{Cell, Grid};
use crate::layout::{CellRect, Layout};
use crate::render::{SessionView, SidebarEntry};
use crate::session::SessionState;

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

/// Pixel format the Metal pipeline + the CAMetalLayer agree on.  sRGB
/// because the AppKit renderer paints in normalised-sRGB-like values
/// (`render.rs` BG/FG constants); matching the colour space here means
/// the same triple drawn through either renderer comes out the same
/// pixel on screen.
const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm_sRGB;

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
    /// Sampler used by the FG fragment shader.  Linear min/mag for
    /// smooth glyph edges, ClampToEdge so sampling outside the
    /// glyph's atlas slot reads padding (transparent) — not the
    /// neighbouring glyph.
    fg_sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// Shared font handling — same data the AppKit renderer uses.
    font: FontCache,
    /// Glyph atlas backing the FG pass.  Constructed in `new` /
    /// `new_headless`; grows lazily as cells reference new glyphs.
    atlas: GlyphAtlas,
    /// Per-frame instance scratch.  Reset at the start of each
    /// `render_layout` so per-frame allocations stay zero in the
    /// steady state.
    cells_scratch: Vec<CellInstance>,
    glyphs_scratch: Vec<GlyphInstance>,
    /// Window-level focus.  Mirror of the AppKit renderer's flag —
    /// drives whether the focused-session cursor is filled or hollow.
    window_focused: bool,
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
        let fg_sampler = build_fg_sampler(&device)?;
        let font = FontCache::build()?;
        // 1024×1024 R8 atlas = 1 MiB.  Fits ~1500 Menlo 13pt 2× glyphs;
        // huge headroom for the realistic working set of a few hundred
        // unique characters.  Bounded — get_or_rasterize returns None
        // on full and the renderer skips the glyph that frame.
        let atlas = GlyphAtlas::new(&device, 1024, 1024)?;

        let layer = unsafe { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(TARGET_FORMAT);
            // framebufferOnly = true: drawables can only be render
            // targets, not sample sources.  Cheaper and we don't need
            // to read pixels back during compositing.
            layer.setFramebufferOnly(true);
            layer.setContentsScale(scale as f64);
        }

        view.setWantsLayer(true);
        unsafe {
            view.setLayer(Some(&layer));
        }

        Ok(Self {
            device,
            queue,
            layer: Some(layer),
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
            fg_pipeline,
            fg_sampler,
            font,
            atlas,
            cells_scratch: Vec::new(),
            glyphs_scratch: Vec::new(),
            window_focused: true,
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
        let fg_sampler = build_fg_sampler(&device)?;
        let font = FontCache::build()?;
        // 1024×1024 R8 atlas = 1 MiB.  Fits ~1500 Menlo 13pt 2× glyphs;
        // huge headroom for the realistic working set of a few hundred
        // unique characters.  Bounded — get_or_rasterize returns None
        // on full and the renderer skips the glyph that frame.
        let atlas = GlyphAtlas::new(&device, 1024, 1024)?;
        Ok(Self {
            device,
            queue,
            layer: None,
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
            fg_pipeline,
            fg_sampler,
            font,
            atlas,
            cells_scratch: Vec::new(),
            glyphs_scratch: Vec::new(),
            window_focused: true,
        })
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        self.font.cell_dims()
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
        // Cast Retained<dyn CAMetalDrawable> down to MTLDrawable for present.
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        cmd.presentDrawable(mtl_drawable);
        cmd.commit();
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
            ref fg_sampler,
            ref mut font,
            ref mut atlas,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            window_focused,
            width_px,
            height_px,
            ..
        } = *self;

        cells_scratch.clear();
        glyphs_scratch.clear();
        build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            font,
            atlas,
            cells_scratch,
            glyphs_scratch,
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
            fg_pipeline,
            fg_sampler,
            atlas,
            device,
            cells_scratch,
            glyphs_scratch,
            width_px as f32,
            height_px as f32,
        );

        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        cmd.presentDrawable(mtl_drawable);
        cmd.commit();
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

        let Self {
            ref device,
            ref queue,
            ref bg_pipeline,
            ref fg_pipeline,
            ref fg_sampler,
            ref mut font,
            ref mut atlas,
            ref mut cells_scratch,
            ref mut glyphs_scratch,
            window_focused,
            ..
        } = *self;

        cells_scratch.clear();
        glyphs_scratch.clear();
        build_instances(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            font,
            atlas,
            cells_scratch,
            glyphs_scratch,
        );

        let cmd = match queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
        encode_passes(
            &cmd,
            target,
            bg_pipeline,
            fg_pipeline,
            fg_sampler,
            atlas,
            device,
            cells_scratch,
            glyphs_scratch,
            width_px as f32,
            height_px as f32,
        );
        cmd.commit();
        unsafe { cmd.waitUntilCompleted() };
    }
}

/// Encode BG + FG passes against `target`.  Shared by the live
/// `render_layout` (drawable target) and the offscreen
/// `render_layout_to_texture` (caller-provided target).
#[allow(clippy::too_many_arguments)]
fn encode_passes(
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    target: &ProtocolObject<dyn MTLTexture>,
    bg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    fg_sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas: &GlyphAtlas,
    device: &ProtocolObject<dyn MTLDevice>,
    cells: &[CellInstance],
    glyphs: &[GlyphInstance],
    viewport_w: f32,
    viewport_h: f32,
) {
    let viewport: [f32; 2] = [viewport_w, viewport_h];

    // BG pass.
    let bg_pass = unsafe { MTLRenderPassDescriptor::new() };
    unsafe {
        let attachments = bg_pass.colorAttachments();
        let color = attachments.objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setLoadAction(MTLLoadAction::Clear);
        color.setStoreAction(MTLStoreAction::Store);
        color.setClearColor(MTLClearColor {
            red: GUTTER.0 as f64,
            green: GUTTER.1 as f64,
            blue: GUTTER.2 as f64,
            alpha: 1.0,
        });
    }
    let bg_buffer = make_instance_buffer(device, cells_as_bytes(cells));
    let bg_encoder = cmd
        .renderCommandEncoderWithDescriptor(&bg_pass)
        .expect("bg encoder");
    bg_encoder.setRenderPipelineState(bg_pipeline);
    if let Some(buf) = &bg_buffer {
        unsafe {
            bg_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
        }
    }
    unsafe {
        bg_encoder.setVertexBytes_length_atIndex(
            NonNull::new(viewport.as_ptr() as *mut c_void).unwrap(),
            std::mem::size_of::<[f32; 2]>(),
            1,
        );
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

    // FG pass.
    let fg_pass = unsafe { MTLRenderPassDescriptor::new() };
    unsafe {
        let attachments = fg_pass.colorAttachments();
        let color = attachments.objectAtIndexedSubscript(0);
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
        unsafe {
            fg_encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
        }
    }
    unsafe {
        fg_encoder.setVertexBytes_length_atIndex(
            NonNull::new(viewport.as_ptr() as *mut c_void).unwrap(),
            std::mem::size_of::<[f32; 2]>(),
            1,
        );
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
    unsafe {
        descriptor.setUsage(
            objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
        );
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Private);
    }
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
}

/// Chrome / cursor / focus-outline constants, kept in sync with
/// `render.rs`.  Shaders consume `f32`s, so duplicate as `f32`-tuples
/// here rather than convert per call.
const GUTTER: (f32, f32, f32) = (0.02, 0.03, 0.06);
const SIDEBAR_BG_F: (f32, f32, f32) = (0.08, 0.10, 0.14);
const FOCUS_OUTLINE: (f32, f32, f32) = (0.30, 0.55, 0.95);
const CURSOR_FG: (f32, f32, f32) = (0.92, 0.92, 0.92);

const SIDEBAR_DOT_R: f32 = 4.5;
const SIDEBAR_LEFT_PAD: f32 = 14.0;
const SIDEBAR_TOP_PAD: f32 = 14.0;
const SIDEBAR_ROW_H: f32 = 22.0;
const SIDEBAR_DOT_LABEL_GAP: f32 = 10.0;
const SIDEBAR_TEXT_FG: (f32, f32, f32) = (0.78, 0.82, 0.88);
const SIDEBAR_FOCUSED_BG: (f32, f32, f32) = (0.13, 0.18, 0.30);
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
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let cell_w = font.cell_w as f32;
    let cell_h = font.cell_h as f32;
    let ascent = font.ascent as f32;
    let (atlas_w, atlas_h) = atlas.dims();
    let atlas_w_f = atlas_w as f32;
    let atlas_h_f = atlas_h as f32;

    // Sidebar BG over the gutter clear.  Painted before the per-
    // session BG so the session rect can overpaint cleanly.
    if layout.sidebar_w > 0.0 {
        let h = layout
            .cells
            .iter()
            .map(|c| c.y_top + c.h)
            .fold(0.0_f64, f64::max)
            .max(1.0);
        cells.push(CellInstance {
            origin: [0.0, 0.0],
            size: [layout.sidebar_w as f32, h as f32],
            color: [SIDEBAR_BG_F.0, SIDEBAR_BG_F.1, SIDEBAR_BG_F.2, 1.0],
        });
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
            cells,
            glyphs,
        );
    }

    if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
        push_sidebar(
            sidebar,
            focused_idx,
            layout.sidebar_w as f32,
            cell_w,
            ascent,
            atlas_w_f,
            atlas_h_f,
            font,
            atlas,
            cells,
            glyphs,
        );
    }
}

/// Sidebar rows.  Per row: optional focus-bg highlight, a state dot
/// (rendered as a small square — TODO: a circle shader for v2),
/// and the label glyphs.  Mirrors `Renderer::draw_sidebar` in
/// `render.rs` so click hit-testing on the same constants lands on
/// the same pixels.
#[allow(clippy::too_many_arguments)]
fn push_sidebar(
    entries: &[SidebarEntry],
    focused_idx: usize,
    sidebar_w: f32,
    cell_w: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
) {
    for (i, entry) in entries.iter().enumerate() {
        let row_top_y = SIDEBAR_TOP_PAD + i as f32 * SIDEBAR_ROW_H;

        if i == focused_idx {
            cells.push(CellInstance {
                origin: [0.0, row_top_y],
                size: [sidebar_w, SIDEBAR_ROW_H],
                color: [
                    SIDEBAR_FOCUSED_BG.0,
                    SIDEBAR_FOCUSED_BG.1,
                    SIDEBAR_FOCUSED_BG.2,
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
        // Square-shaped dot — circle would need a discard-on-radius
        // fragment shader; visual fidelity bump is phase 5e+.
        cells.push(CellInstance {
            origin: [dot_cx - SIDEBAR_DOT_R, dot_cy - SIDEBAR_DOT_R],
            size: [SIDEBAR_DOT_R * 2.0, SIDEBAR_DOT_R * 2.0],
            color: [dot_color.0, dot_color.1, dot_color.2, 1.0],
        });

        // Label text.  Lay out monospace via cell_w (sidebar labels
        // are ASCII / short tmux names, so cell_w accuracy is fine).
        let label_x = dot_cx + SIDEBAR_DOT_R + SIDEBAR_DOT_LABEL_GAP;
        let baseline_y = dot_cy + ascent * 0.40 - SIDEBAR_ROW_H * 0.20;
        let mut x = label_x;
        for ch in entry.label.chars() {
            let (font_idx, glyph) = font.resolve_char(ch, false, false);
            if glyph != 0 {
                let ct_font = font.font(font_idx).clone();
                if let Some(e) = atlas.get_or_rasterize(
                    GlyphKey {
                        font_id: font_idx as u32,
                        glyph,
                    },
                    &ct_font,
                ) {
                    glyphs.push(GlyphInstance {
                        origin: [x + e.bearing_x as f32, baseline_y - e.bearing_y as f32],
                        size: [e.px_w as f32, e.px_h as f32],
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
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
) {
    // Terminal-bg fill for the rect.
    cells.push(CellInstance {
        origin: [rect.x as f32, rect.y_top as f32],
        size: [rect.w as f32, rect.h as f32],
        color: [BG.0 as f32, BG.1 as f32, BG.2 as f32, 1.0],
    });

    let grid = view.grid;
    let cols = grid.cols() as usize;

    for r in 0..grid.rows() {
        let row_y = rect.y_top as f32 + (r as f32) * cell_h;
        let baseline_y = row_y + ascent;

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
                origin: [rect.x as f32 + start as f32 * cell_w, row_y],
                size: [(c - start) as f32 * cell_w, cell_h],
                color: [bg.0 as f32, bg.1 as f32, bg.2 as f32, 1.0],
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
            let cell = grid.cell_at_view(view.view_offset, c as u16, r);
            if cell.ch == ' ' || cell.ch == '\0' {
                continue;
            }
            let (font_idx, glyph) =
                font.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
            if glyph == 0 {
                continue;
            }
            let ct_font = font.font(font_idx).clone();
            let entry = match atlas.get_or_rasterize(
                GlyphKey {
                    font_id: font_idx as u32,
                    glyph,
                },
                &ct_font,
            ) {
                Some(e) => e,
                None => continue,
            };
            let fg = resolve_attrs(cell.attrs).0;
            let cell_origin_x = rect.x as f32 + c as f32 * cell_w;
            // bearing_x = horizontal offset from pen to bitmap left.
            // bearing_y = pixels from baseline up to bitmap top — so
            // dest_y (top edge in y-down coords) = baseline - bearing_y.
            let dest_x = cell_origin_x + entry.bearing_x as f32;
            let dest_y = baseline_y - entry.bearing_y as f32;
            glyphs.push(GlyphInstance {
                origin: [dest_x, dest_y],
                size: [entry.px_w as f32, entry.px_h as f32],
                uv0: [
                    entry.u0 as f32 / atlas_w,
                    entry.v0 as f32 / atlas_h,
                ],
                uv1: [
                    entry.u1 as f32 / atlas_w,
                    entry.v1 as f32 / atlas_h,
                ],
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
                origin: [rect.x as f32 + start as f32 * cell_w, underline_y],
                size: [(u - start) as f32 * cell_w, underline_h],
                color: [fg.0 as f32, fg.1 as f32, fg.2 as f32, 1.0],
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
        let cell = grid.cell_at_view(0, col, row);
        if cell.ch != ' ' && cell.ch != '\0' {
            let (font_idx, glyph) =
                font.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
            if glyph != 0 {
                let ct_font = font.font(font_idx).clone();
                if let Some(entry) = atlas.get_or_rasterize(
                    GlyphKey {
                        font_id: font_idx as u32,
                        glyph,
                    },
                    &ct_font,
                ) {
                    let cell_origin_x = rect.x as f32 + col as f32 * cell_w;
                    let row_y = rect.y_top as f32 + (row as f32) * cell_h;
                    let baseline_y = row_y + ascent;
                    let dest_x = cell_origin_x + entry.bearing_x as f32;
                    let dest_y = baseline_y - entry.bearing_y as f32;
                    glyphs.push(GlyphInstance {
                        origin: [dest_x, dest_y],
                        size: [entry.px_w as f32, entry.px_h as f32],
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
        let cx = rect.x as f32 + col as f32 * cell_w;
        let cy = rect.y_top as f32 + row as f32 * cell_h;
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

    // Focus outline.
    if view.focused {
        let stroke = (cell_h * 0.10).max(1.0);
        let color = [FOCUS_OUTLINE.0, FOCUS_OUTLINE.1, FOCUS_OUTLINE.2, 1.0];
        let (rx, ry, rw, rh) = (rect.x as f32, rect.y_top as f32, rect.w as f32, rect.h as f32);
        cells.push(CellInstance { origin: [rx, ry], size: [rw, stroke], color });
        cells.push(CellInstance { origin: [rx, ry + rh - stroke], size: [rw, stroke], color });
        cells.push(CellInstance { origin: [rx, ry], size: [stroke, rh], color });
        cells.push(CellInstance { origin: [rx + rw - stroke, ry], size: [stroke, rh], color });
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

    let descriptor = unsafe { MTLRenderPipelineDescriptor::new() };
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    unsafe {
        attachment.setPixelFormat(TARGET_FORMAT);
    }

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

    let descriptor = unsafe { MTLRenderPipelineDescriptor::new() };
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    unsafe {
        attachment.setPixelFormat(TARGET_FORMAT);
        attachment.setBlendingEnabled(true);
        attachment.setRgbBlendOperation(MTLBlendOperation::Add);
        attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (FG) error: {:?}", e))
}

fn build_fg_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let descriptor = unsafe { MTLSamplerDescriptor::new() };
    unsafe {
        descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
        descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
        descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
        descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
    }
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
        unsafe {
            descriptor.setUsage(
                objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
            );
            descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        }
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
        unsafe {
            descriptor.setUsage(
                objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
            );
            descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        }
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
            .get_or_rasterize(GlyphKey { font_id: 0, glyph: cg_glyph }, &font)
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
        use crate::grid::Grid;
        use crate::layout::Layout;

        let device = match system_default_device() {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut font = FontCache::build().expect("font");
        let mut atlas = GlyphAtlas::new(&device, 256, 256).expect("atlas");

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
        };

        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            &mut font,
            &mut atlas,
            &mut cells,
            &mut glyphs,
        );

        // Expected glyph instances:
        //  - 'B' at col 1 (FG colour) — the cursor cell at col 0 is
        //    skipped during the FG emit because it's solid-cursor
        //  - 'A' at col 0 (BG colour) re-rendered after the cursor block
        //  Order in vec is FG-pass-first then cursor re-render, so [B, A].
        //  cells: terminal-bg fill + cursor block + focus outline ×4
        assert!(cells.len() >= 6, "got cells.len()={}", cells.len());
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
}
