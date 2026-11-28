//! One window's frame: build the instances, encode the passes, and either
//! wait for the GPU or leave the frame running.

use std::ptr::NonNull;

use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer, MTLCommandQueue, MTLTexture};

use super::menu_pass;
use super::{encode_passes, take_instance_buffer_cost, take_upload_bytes, Drawing, MetalRenderer, RenderSplit, WindowRender};
use crate::frame_build::frame::{build_frame, FormPanel};
use crate::layout::Layout;
use crate::render::{SessionView, SidebarEntry};

impl MetalRenderer {
    /// Encodes the BG + FG passes against `target` (any Render-Target
    /// MTLTexture) and blocks on `waitUntilCompleted` so the caller can
    /// time the full GPU round-trip without racing the next frame.
    ///
    /// Use case: the `--bench metal-render` headless harness of the
    /// `marspot` dev binary.
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
        let _ = wr.take_clear_required();

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
            ref process_panel,
            ref cc_usage,
            ref settings_panel,
            ref shortcuts_panel,
            ref helpers_panel,
            ref web_images,
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
        let build_stats = build_frame(
            layout,
            views,
            sidebar,
            focused_idx,
            window_focused,
            process_panel.as_ref(),
            cc_usage.as_ref(),
            FormPanel::open(settings_panel.as_ref(), shortcuts_panel.as_ref(), helpers_panel.as_ref()),
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
            &mut wr.kept,
            overlay_cells_scratch,
            overlay_glyphs_scratch,
            overlay_ui_rects_scratch,
            false,
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
        let _ = take_instance_buffer_cost(); // zero the accumulators
        let _ = take_upload_bytes();
        let t_cmdbuf0 = std::time::Instant::now();
        let cmd = match queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
        let t_encode0 = std::time::Instant::now();
        encode_passes(
            &cmd,
            target,
            &Drawing {
                bg_pipeline,
                dot_pipeline,
                fg_pipeline,
                fg_color_pipeline,
                scene_ui_pipeline,
                fg_sampler,
                atlas_texture: atlas_texture.texture(),
                color_atlas_texture: color_atlas_texture.texture(),
                device,
                images: web_images,
            },
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
            // Safe to refill in place: the pool is this window's own,
            // and a window never has two frames in flight — so the
            // frame that last read these buffers has completed.
            Some(&mut wr.instance_pool),
            Some(&mut wr.kept.panes[..]),
            ui_slab,
        );
        let t_canvas0 = std::time::Instant::now();
        // The context menu, over the finished frame: see `menu_pass`.
        let viewport_px = [width_px as f32, height_px as f32];
        if let Some(menu) = self.context_menu_state.as_ref() {
            menu_pass::encode_menu(
                &mut menu.surface.borrow_mut(),
                target,
                &cmd,
                scene_ui_pipeline,
                fg_pipeline,
                fg_sampler,
                atlas_texture.texture(),
                device,
                &viewport_px,
            );
        }
        let t_commit0 = std::time::Instant::now();
        let (instbuf_us, instbuf_bytes) = take_instance_buffer_cost();
        if let (false, Some(done)) = (block, self.frame_done.as_ref()) {
            // SAFETY: a live block of the handler's signature; Metal
            // retains it until it has been called
            unsafe { cmd.addCompletedHandler(block2::RcBlock::as_ptr(done)) };
        }
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
            upload_bytes: take_upload_bytes(),
            canvas_us: (t_commit0 - t_canvas0).as_micros() as u64,
            gpu_wait_us: (t_end - t_gpu0).as_micros() as u64,
            gpu_exec_us,
            build_panes_us: build_stats.panes_us,
            panes_rebuilt: build_stats.rebuilt,
            panes_total: build_stats.considered,
            rows_rebuilt: build_stats.rows_rebuilt,
            rows_total: build_stats.rows_total,
            glyphs_rasterised,
            evictions,
            rebuilds,
        };
    }

    /// Have `wake` called, on a thread of Metal's, each time a frame
    /// left running by `render_layout_to_texture_async` completes: the
    /// caller's cue to ask `WindowRender::settled()`.
    pub fn set_frame_done(&mut self, wake: impl Fn() + Send + Sync + 'static) {
        self.frame_done = Some(block2::RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| wake()));
    }

    /// Where the last IOSurface frame spent its time.
    pub fn last_render_split(&self) -> RenderSplit {
        self.last_render_split
    }
}

impl super::WindowRender {
    /// What the window's lone pane shows in its place, for its header;
    /// empty when it shows itself or the window has more panes.
    /// Whether that is new.
    pub fn set_view_line(&mut self, text: &str) -> bool {
        self.kept.chrome.view_line.set(text)
    }
}
