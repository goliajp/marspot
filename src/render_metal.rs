// CARVE-OUT: pre-existing god file: the Metal renderer; to be split by concern; shrink on every touch
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
mod tests {
    use super::*;
    use crate::frame_build::text_run::preedit_placements;
    use crate::frame_build::color::rgba8_of_f32;
    use crate::frame_build::frame::{
        attention_scrim, DRAG_SOURCE_SCRIM, EMPTY_SEAT_SCRIM, PARKED_SCRIM, RESTING_SCRIM,
        UNFOCUSED_SCRIM,
    };







    /// Verify the basic Metal plumbing works on this machine — proves
    /// the dep + bindings resolve and we can talk to the GPU.  CI on
    /// non-Metal machines will skip this naturally because
    /// `system_default_device` returns `Err`.
    #[test]
    fn headless_renderer_constructs() {
        match MetalRenderer::new_headless() {
            Ok(r) => {
                assert_eq!((r.width_px, r.height_px), (0.0, 0.0));
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
        // A glyph's soft edge over an opaque target leaves it opaque:
        // the window server composites the surface as it is.
        let soft = bytes.chunks_exact(4).filter(|p| p[0] > 8 && p[0] < 247).count();
        assert!(soft > 0, "no soft edge in the picture to judge by");
        assert!(bytes.chunks_exact(4).all(|p| p[3] == 255), "alpha under one where a glyph's edge was blended");

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
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            title: "", selection: None, ime_preedit: "",
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[], search_overlay: None, history: Default::default(), seq: 0,
            dormant: false, recede: 0, scrim: 0.0,
        };
        let mut overlay_rects = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None, None,
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
            &mut crate::frame_build::frame::FrameKeep::default(),
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

    /// The scrim primitive is shared by every reason a pane recedes,
    /// and they do not stack: two 0.22 layers composite to 0.39, a
    /// shade nobody chose.
    #[test]
    fn scrims_do_not_stack_the_deepest_reason_wins() {
        use crate::layout::Layout;
        let device = match system_default_device() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            title: "", selection: None, ime_preedit: "",
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[], search_overlay: None, history: Default::default(), seq: 0,
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
                None, None, None, None, None, None, None,
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
                &mut crate::frame_build::frame::FrameKeep::default(),
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
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
            seq: 0,
        };
        let mut run = |v: &SessionView| -> Vec<GlyphInstance> {
            let mut glyphs: Vec<GlyphInstance> = Vec::new();
            build_instances(
                &layout, std::slice::from_ref(v), &[], 0, true,
                None, None, None, None, None, None, None,
                &mut font, &mut atlas, &mut color_atlas,
                &mut Vec::new(), &mut glyphs, &mut Vec::new(), &mut Vec::new(),
                &mut Vec::new(), &mut crate::frame_build::frame::FrameKeep::default(),
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
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            dormant: false,
            recede: 0,
            scrim,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
            seq: 0,
        };
        // Pane 0 focused; its right / bottom / bottom-right neighbours
        // all carry a scrim, which is the situation that hid the ring.
        let views = vec![mk(true, 0.0), mk(false, 0.35), mk(false, 0.35), mk(false, 0.35)];

        let mut overlay_ui: Vec<UiRectInstance> = Vec::new();
        build_instances(
            &layout, &views, &[], 0, true,
            None, None, None, None, None, None, None,
            &mut font, &mut atlas, &mut color_atlas,
            // cells, glyphs, color_glyphs, _dots
            &mut Vec::new(), &mut Vec::new(), &mut Vec::new(), &mut Vec::new(),
            // ui_rects, pane_caches
            &mut Vec::new(), &mut crate::frame_build::frame::FrameKeep::default(),
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
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut crate::frame_build::frame::FrameKeep::default(),
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
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut crate::frame_build::frame::FrameKeep::default(),
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: top_fixed,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut crate::frame_build::frame::FrameKeep::default(),
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
            history: Default::default(),
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: view.right_badge, agent_tui: view.agent_tui,
            cwd: "",
            top_fixed_h_cells: view.top_fixed_h_cells,
            bot_fixed_h_cells: view.bot_fixed_h_cells,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
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
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
                &mut Vec::new(),
                &mut crate::frame_build::frame::FrameKeep::default(),
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
            .filter(|c| c.color == crate::ui::theme::terminal::TermColors::current().search_hit)
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
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &spans,
            search_overlay: None,
            history: Default::default(),
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
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut Vec::new(),
            &mut crate::frame_build::frame::FrameKeep::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
        );
        // No HIGHLIGHT_BG cell should be present.
        let highlight_count = cells
            .iter()
            // Eight bits now, so the colour is matched as the byte it
            // becomes rather than as the float it was written as.
            .filter(|c| c.color == crate::ui::theme::terminal::TermColors::current().search_hit)
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
            title: "", selection: None, ime_preedit: "", dormant: false,
                                                                                recede: 0,
                scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
            seq: 0,
        };
        let view_bot = SessionView {
            grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
            title: "", selection: None, ime_preedit: "", dormant: false,
                                                                                recede: 0,
                scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 2,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
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
                &mut font,
                &mut atlas,
                &mut color_atlas,
                &mut cells,
                &mut glyphs,
                &mut color_glyphs,
                &mut dots,
                &mut Vec::new(),
                &mut crate::frame_build::frame::FrameKeep::default(),
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
    fn sheet_alpha_blends_at_the_requested_opacity() {
        use super::sheet::Sheet;
        use crate::ui::core::Color;
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
        };
        let (w, h) = (16u32, 16u32);
        for (alpha, want) in [(1.0_f64, 255.0_f64), (0.5, 127.5), (0.25, 63.75)] {
            let mut sheet = Sheet::new(1.0);
            sheet.rect()
                .at(0.0, 0.0)
                .size(w as f64, h as f64)
                .fill(Color::rgba(255, 255, 255, alpha))
                .draw();
            let bytes = renderer
                .render_sheet_to_bitmap(w, h, &sheet, 48.0, 12.0, 9.0, false)
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
    fn sheet_same_pipeline_submission_order_is_z_order() {
        use super::sheet::Sheet;
        use crate::ui::core::Color;
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
        };
        let w = 8u32;
        let h = 8u32;
        let mut sheet = Sheet::new(1.0);
        // All three at (0, 0) sized full window.  Same pipeline
        // (Rect), so the encoder draws them in array order.
        sheet.rect()
            .at(0.0, 0.0)
            .size(w as f64, h as f64)
            .fill(Color::rgb(255, 0, 0))
            .draw();
        sheet.rect()
            .at(0.0, 0.0)
            .size(w as f64, h as f64)
            .fill(Color::rgb(0, 255, 0))
            .draw();
        sheet.rect()
            .at(0.0, 0.0)
            .size(w as f64, h as f64)
            .fill(Color::rgb(0, 0, 255))
            .draw();

        let bytes = renderer.render_sheet_to_bitmap(w, h, &sheet, 48.0, 12.0, 9.0, false)
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

    /// `sheet_runs` correctly merges consecutive same-kind
    /// primitives into one run and splits at kind transitions.
    #[test]
    fn sheet_runs_batch_same_kind_split_on_transition() {
        use super::sheet::{RunKind, Sheet};
        use crate::ui::core::Color;
        let mut sheet = Sheet::new(1.0);
        sheet.rect().fill(Color::WHITE).draw();
        sheet.rect().fill(Color::BLACK).draw();
        sheet.text(0.0, 0.0, "x").color(Color::WHITE).draw();
        sheet.rect().fill(Color::WHITE).draw();
        // nothing is rendered: only the grouping into runs is read
        let mut renderer = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
        };
        let drawn = renderer.sheet_runs(&sheet, 48.0, 12.0, 9.0, false);
        let (runs, gl) = (&drawn.runs, &drawn.glyphs);
        // Expected sequence: UiRect (2 rects) → Glyph (text) →
        // UiRect (final rect).  Text may produce 0 or 1+ glyphs
        // depending on whether 'x' resolves through the chrome
        // font — guard the assertion against the 0-glyph case
        // by collapsing the middle run.
        assert!(runs.len() >= 2, "expected at least 2 runs, got {runs:?}", runs = runs.len());
        assert_eq!(runs[0].0, RunKind::UiRect);
        assert_eq!(runs[0].1, 2);
        assert_eq!(runs.last().unwrap().0, RunKind::UiRect);
        // If the text produced glyphs, there's a Glyph run in the middle.
        if !gl.is_empty() {
            assert_eq!(runs.len(), 3);
            assert_eq!(runs[1].0, RunKind::Glyph);
            assert_eq!(runs[1].1, gl.len());
        }
    }
}

