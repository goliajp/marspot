//! AppKit-native rendering.
//!
//! Pivoted from custom Metal + glyph atlas + shader pipeline (which had
//! gamma + atlas neighbor + sampling issues we couldn't shake).  This
//! module instead lets macOS handle all glyph rasterization via
//! CoreText into a `CGBitmapContext`, then sets the resulting `CGImage`
//! as the host view's CALayer contents.  CoreAnimation composites it.
//!
//! The visual output should now match `Terminal.app`/`TextEdit` pixel
//! for pixel — same font hinting, same antialiasing, same gamma path
//! Apple uses everywhere else.

use crate::font_cache::{resolve_attrs, FontCache, BG, FG};
use crate::grid::{Cell, Grid};
use crate::layout::{CellRect, Layout};
use crate::session::SessionState;
use core_graphics::base::{
    kCGBitmapByteOrder32Big, kCGImageAlphaNone, kCGImageAlphaPremultipliedLast, CGFloat,
};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGInterpolationQuality, CGTextDrawingMode};
use core_graphics::font::CGGlyph;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::image::CGImage;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSColor, NSView};
use objc2_quartz_core::{kCAGravityTopLeft, CALayer};
/// The dominant chrome surface — sidebar, header strip, unfocused
/// cell rects.  Mirror of `render_metal::BG_PANEL`.
const BG_PANEL: (CGFloat, CGFloat, CGFloat) = (0.022, 0.028, 0.042);
/// Sidebar background = panel surface (the whole chrome region).
const SIDEBAR_BG: (CGFloat, CGFloat, CGFloat) = BG_PANEL;
/// The deeper "this is the canvas I'm typing into" surface used
/// for the focused 9-grid cell AND the focused sidebar row.
/// Mirror of `render_metal::BG_FOCUSED`.
const BG_FOCUSED: (CGFloat, CGFloat, CGFloat) = (0.006, 0.008, 0.014);
/// Hair-darker than `BG_PANEL` — reads as a quiet depression line
/// between adjacent panels.  One uniform tone for sidebar↔grid,
/// header↔grid, and cell↔cell seams.  Mirror of
/// `render_metal::SEAM`.
const SEAM: (CGFloat, CGFloat, CGFloat) = (0.055, 0.062, 0.075);

/// Per-session render parameters.  Caller bundles the relevant bits
/// so the renderer doesn't need to know about Session, Marspot, or
/// MarspotEvent — anything that can produce a Grid + view offset can
/// drive a render.
pub struct SessionView<'a> {
    pub grid: &'a Grid,
    /// Lines scrolled up from live (0 = live view).
    pub view_offset: u16,
    /// DECTCEM (?25) — false ⇒ hide cursor for this session.
    pub cursor_visible: bool,
    /// True for the session that owns the keyboard right now.
    /// Drives the focus outline + cursor style (filled vs hollow).
    pub focused: bool,
    /// Short label drawn in the title strip at the top of the
    /// cell.  Empty string skips the strip entirely (useful for
    /// snapshot / mcli single-session rendering).
    pub title: &'a str,
    /// Optional text selection for this session in the LIVE grid
    /// (`(anchor_col, anchor_row, focus_col, focus_row)` in cell
    /// coordinates).  `None` means no selection.  When set, the
    /// renderer paints a SELECTION_BG highlight over the cells
    /// in [start..=end] (row-major) inside the terminal area.
    pub selection: Option<((u16, u16), (u16, u16))>,
}

/// One row of the sidebar — what the user sees on the left.  Length
/// of the slice passed to [`Renderer::render_layout`] should match
/// `views.len()`; entry `i` describes session `i`.
pub struct SidebarEntry<'a> {
    /// Short label drawn after the state dot, e.g. `"1"`, `"build"`.
    pub label: &'a str,
    /// Drives the colour of the state dot.
    pub state: SessionState,
}

/// Sidebar typography (in points-equivalent pixels at scale = 1).
/// Tuned for the default 200-pt-wide sidebar.
const SIDEBAR_DOT_R: f64 = 4.5;
const SIDEBAR_LEFT_PAD: f64 = 14.0;
// SIDEBAR_TOP_PAD is now a per-Layout value (`layout::sidebar_top_pad_phys`)
// so it stays in lockstep with the [+] add-session button band; row
// height is still a fixed phys constant.
const SIDEBAR_ROW_H: f64 = 22.0;
/// Gap between the dot's right edge and the start of the label text.
const SIDEBAR_DOT_LABEL_GAP: f64 = 10.0;
const SIDEBAR_TEXT_FG: (CGFloat, CGFloat, CGFloat) = (0.78, 0.82, 0.88);
// Sidebar focused row reuses the cell focused tone — one
// "selected" affordance across the whole UI.
const SIDEBAR_FOCUSED_BG: (CGFloat, CGFloat, CGFloat) = BG_FOCUSED;
const STATE_ACTIVE: (CGFloat, CGFloat, CGFloat) = (0.30, 0.85, 0.45);
const STATE_IDLE: (CGFloat, CGFloat, CGFloat) = (0.55, 0.58, 0.62);
const STATE_EXITED: (CGFloat, CGFloat, CGFloat) = (0.85, 0.30, 0.30);

pub struct Renderer {
    /// When attached to an NSView, we update its CALayer's `contents` on
    /// each render with a fresh CGImage.  In headless mode this is None.
    layer: Option<Retained<CALayer>>,
    /// Shared font + glyph + colour resolution — see `font_cache.rs`.
    /// `font.cell_w/cell_h/ascent` are this renderer's metrics; we
    /// re-export them through `cell_dims()` for callers (main.rs).
    font: FontCache,
    viewport_w: f64,
    viewport_h: f64,
    /// Physical pixels of top inset reserved above the grid for window
    /// chrome (macOS traffic-light buttons). Single-session callers
    /// (mcli) set this via `set_top_inset` so `render(view)` builds a
    /// 1×1 Layout that pushes the grid below the chrome. Multi-session
    /// callers (marspot) build their own Layout with `top_inset` baked
    /// in and call `render_layout` directly — they bypass this field.
    top_inset_phys: f64,
    /// Window-level focus.  When false, even the focused-session
    /// cursor draws hollow because the user clearly isn't typing
    /// into marspot.
    window_focused: bool,
    /// Reused CGBitmapContext for the layer-attached path.  Held as
    /// `(ctx, width_px, height_px)` and rebuilt only when the viewport
    /// resizes — `CGBitmapContextCreate` is ~50 µs of pure overhead.
    /// CGImage from CGBitmapContextCreateImage is COW against this
    /// buffer, so the previous frame's image stays valid for the
    /// CALayer until our next draw triggers the copy.
    bitmap_ctx: Option<(CGContext, u32, u32)>,
    /// Per-frame scratch buffers, lifted out of the inner loops so the
    /// per-row glyph runs and per-cell decode don't allocate.  Wrapped
    /// in one struct so `mem::take` cleanly borrow-swaps it past the
    /// `&mut self` we hold for `resolve_char` etc.
    scratch: Scratch,
    /// Per-cell rasterised masks for box-drawing (U+2500..U+257F) and
    /// block elements (U+2580..U+259F). Key is (char codepoint, cell-w
    /// in px, cell-h in px). Value is an 8-bit grayscale CGImage used
    /// as a clip mask — fg colour is set per blit so the same mask
    /// covers every colour the TUI uses.  Caching matters because
    /// `CGBitmapContextCreate` is ~50 µs each — without it, drawing 50+
    /// box chars per frame becomes the dominant cost.  alacritty/kitty
    /// take the same approach (per-glyph rasterisation into a sprite
    /// atlas) — it's the only path that guarantees corners join with
    /// no sub-pixel drift, because arms are composed in cell-local
    /// coords that don't depend on the cell's absolute screen origin.
    box_mask_cache: std::collections::HashMap<(u32, u16, u16), CGImage>,
}

type RgbF = (CGFloat, CGFloat, CGFloat);

#[derive(Default)]
struct Scratch {
    /// One glyph run inside a row, refilled per run.
    run_glyphs: Vec<CGGlyph>,
    run_positions: Vec<CGPoint>,
    /// One full row of cells, decoded once and read by all three
    /// passes (bg / fg / underline).  `row_attrs[i]` is the resolved
    /// `(fg, bg)` for `row_cells[i]`.
    row_cells: Vec<Cell>,
    row_attrs: Vec<(RgbF, RgbF)>,
}

impl Renderer {
    pub fn new(view: &NSView, scale: f32) -> Result<Self, String> {
        Self::build(Some(view), scale)
    }

    pub fn new_offscreen(scale: f32) -> Result<Self, String> {
        Self::build(None, scale)
    }

    fn build(view: Option<&NSView>, scale: f32) -> Result<Self, String> {
        let font = FontCache::build()?;

        let layer = if let Some(view) = view {
            view.setWantsLayer(true);
            // The view is now layer-backed; AppKit creates a default
            // CALayer for us.  We grab it and set its contentsScale so
            // CA composites at the right density.
            let layer = unsafe {
                view.layer()
                    .ok_or("layer not available on view".to_string())?
            };
            unsafe {
                layer.setContentsScale(scale as f64);
                // Default contentsGravity is `resize` — during a live
                // resize the previous frame's CGImage gets stretched to
                // the new layer bounds before our redraw lands, which
                // looks like the BG flickering / changing colour.  Pin
                // the existing image to the top-left and let the BG show
                // through the gap until the next frame is ready.
                layer.setContentsGravity(kCAGravityTopLeft);
                // The terminal is fully opaque; tell CA so it can skip
                // compositing anything underneath.
                layer.setOpaque(true);
            }
            // Window BG = panel tone, the dominant chrome surface.
            // The resize-flicker zone shows BG_PANEL during live
            // resize, smoothly continuous with the rendered frame.
            unsafe {
                if let Some(window) = view.window() {
                    let bg = NSColor::colorWithSRGBRed_green_blue_alpha(
                        BG_PANEL.0, BG_PANEL.1, BG_PANEL.2, 1.0,
                    );
                    window.setBackgroundColor(Some(&bg));
                }
            }
            Some(layer)
        } else {
            None
        };

        Ok(Self {
            layer,
            font,
            viewport_w: 0.0,
            viewport_h: 0.0,
            top_inset_phys: 0.0,
            window_focused: true,
            bitmap_ctx: None,
            scratch: Scratch::default(),
            box_mask_cache: std::collections::HashMap::new(),
        })
    }

    /// Reserve a top strip (physical pixels) above the grid so window
    /// chrome (traffic lights, focused-session status, …) doesn't paint
    /// over terminal content. Single-session callers set this once at
    /// `resumed` from `HEADER_PT * scale`. Multi-session callers route
    /// the same value through `Layout::build`'s `top_inset` param and
    /// don't need to touch this.
    pub fn set_top_inset(&mut self, phys: f64) {
        self.top_inset_phys = phys;
    }

    /// Top inset in physical pixels currently in effect. mcli reads this
    /// in `resized()` to subtract from window height before computing
    /// row count, so the session grid doesn't request rows that would
    /// be clipped under the chrome strip.
    pub fn top_inset_phys(&self) -> f64 {
        self.top_inset_phys
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }


    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        self.viewport_w = width_px;
        self.viewport_h = height_px;
    }

    /// Cell dimensions in physical pixels — the unit the renderer uses
    /// internally.  Callers (e.g. the resize path) divide the viewport by
    /// these to get the grid dimensions that fit.
    pub fn cell_dims(&self) -> (f64, f64) {
        self.font.cell_dims()
    }

    /// AppKit path has no glyph atlas (CGImage drawn through CT each
    /// frame); reported as 0 for symmetry with the Metal renderer's
    /// MARSPOT_PROFILE_RSS instrumentation.
    pub fn atlas_approx_bytes(&self) -> usize {
        0
    }

    pub fn fontcache_approx_bytes(&self) -> usize {
        self.font.approx_bytes()
    }

    /// AppKit path uses no Metal buffers; reported as 0.
    pub fn metal_buffers_approx_bytes(&self) -> usize {
        0
    }

    /// Render a single session full-window (mcli + the snapshot bench).
    /// Convenience wrapper over [`render_layout`](Self::render_layout)
    /// using a 1-cell layout that fills the viewport.
    pub fn render(&mut self, view: SessionView) {
        if self.layer.is_none() {
            return;
        }
        if self.viewport_w < 1.0 || self.viewport_h < 1.0 {
            return;
        }
        let layout = Layout::build(
            self.viewport_w,
            self.viewport_h,
            0.0,
            self.top_inset_phys,
            0.0,
            1,
            1,
            self.font.cell_w,
            self.font.cell_h,
        );
        self.render_layout(&layout, std::slice::from_ref(&view), &[], 0);
    }

    /// Render N session views + a sidebar into a window-sized frame.
    /// Each view's rect is taken from `layout.cells[i]`; cells beyond
    /// `views.len()` stay as the default frame background (so a
    /// partially-filled grid reads as empty cells).  When `sidebar`
    /// is empty no chrome text is drawn (mcli passes `&[]`).
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
        if self.viewport_w < 1.0 || self.viewport_h < 1.0 {
            return;
        }
        let total_w = self.viewport_w as u32;
        let total_h = self.viewport_h as u32;
        let ctx = self.frame_context(total_w, total_h, layout);
        // Borrow-swap the scratch out of self so per-cell decode doesn't
        // alias the `&mut self` the inner methods need for resolve_char
        // / char_cache mutation.
        let mut scratch = std::mem::take(&mut self.scratch);
        for (i, view) in views.iter().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                self.draw_session_in_rect(&ctx, total_h, rect, view, &mut scratch);
            }
        }
        if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
            self.draw_sidebar(&ctx, total_h, layout, sidebar, focused_idx);
        }
        self.scratch = scratch;
        let cgimage = ctx
            .create_image()
            .expect("CGContext should produce a CGImage");
        let layer = self.layer.as_ref().unwrap();
        let cg_ptr = cgimage.as_ptr() as *const AnyObject;
        unsafe {
            layer.setContents(Some(&*cg_ptr));
        }
    }

    /// Headless-friendly variant: render a single session into a
    /// freshly-allocated bitmap context at `width × height` physical
    /// pixels and return the raw bytes (PNG wants RGBA; we swap
    /// R↔B per pixel to honour that).
    pub fn snapshot(
        &mut self,
        width: u32,
        height: u32,
        grid: &Grid,
    ) -> Result<Vec<u8>, String> {
        self.viewport_w = width as f64;
        self.viewport_h = height as f64;
        let layout = Layout::build(
            width as f64,
            height as f64,
            0.0,
            0.0,
            0.0,
            1,
            1,
            self.font.cell_w,
            self.font.cell_h,
        );
        let view = SessionView {
            grid,
            view_offset: 0,
            cursor_visible: true,
            focused: true,
            title: "",
            selection: None,
        };
        let mut ctx = self.frame_context(width, height, &layout);
        let mut scratch = std::mem::take(&mut self.scratch);
        if let Some(rect) = layout.cells.first() {
            self.draw_session_in_rect(&ctx, height, rect, &view, &mut scratch);
        }
        self.scratch = scratch;
        let mut bytes = ctx.data().to_vec();
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
        Ok(bytes)
    }

    /// Acquire a CGBitmapContext sized to the viewport, filled with the
    /// chrome background (gutter + sidebar) so per-session cells only
    /// have to paint their own backgrounds.  Reuses `self.bitmap_ctx`
    /// when the dims match — `CGBitmapContextCreate` is ~50 µs floor
    /// of pure overhead on every frame.  Returns a clone (CFRetain,
    /// few ns) so the caller can hand it around without holding a
    /// borrow on `self`.
    fn frame_context(&mut self, width: u32, height: u32, layout: &Layout) -> CGContext {
        let ctx = match &self.bitmap_ctx {
            Some((c, w, h)) if *w == width && *h == height => c.clone(),
            _ => {
                let space = CGColorSpace::create_device_rgb();
                let row_bytes = width as usize * 4;
                let bitmap_info = kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big;
                let new_ctx = CGContext::create_bitmap_context(
                    None,
                    width as usize,
                    height as usize,
                    8,
                    row_bytes,
                    &space,
                    bitmap_info,
                );
                self.bitmap_ctx = Some((new_ctx.clone(), width, height));
                new_ctx
            }
        };

        // One dark surface plus uniformly weak SEAM hairlines on
        // every internal boundary: sidebar↔grid, header↔grid, and
        // cell↔cell — same colour, same width, same opacity.
        ctx.set_rgb_fill_color(SIDEBAR_BG.0, SIDEBAR_BG.1, SIDEBAR_BG.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width as f64, height as f64),
        ));
        if layout.gutter > 0.0 && !layout.cells.is_empty() {
            let h_total = height as f64;
            let avail_h = layout.window_h - layout.top_inset;
            let y_bottom_avail = h_total - layout.top_inset - avail_h;
            ctx.set_rgb_fill_color(SEAM.0, SEAM.1, SEAM.2, 1.0);
            // Sidebar↔grid vertical seam.
            if layout.sidebar_w > 0.0 {
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(layout.sidebar_w, y_bottom_avail),
                    &CGSize::new(layout.gutter, avail_h),
                ));
            }
            // Header↔grid horizontal seam (full width, just below
            // the top inset in y-down terms).
            if layout.top_inset > 0.0 {
                let y_b = h_total - layout.top_inset;
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(0.0, y_b),
                    &CGSize::new(layout.window_w, layout.gutter),
                ));
            }
            // Inter-cell vertical seams.
            for c in 1..layout.grid_cols {
                let prev = layout.cells[c - 1];
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(prev.x + prev.w, y_bottom_avail),
                    &CGSize::new(layout.gutter, avail_h),
                ));
            }
            // Inter-cell horizontal seams.
            let grid_left = layout
                .cells
                .first()
                .map(|c| c.x)
                .unwrap_or(layout.sidebar_w);
            let grid_right = layout
                .cells
                .last()
                .map(|c| c.x + c.w)
                .unwrap_or(layout.window_w);
            let avail_w_grid = grid_right - grid_left;
            for r in 1..layout.grid_rows {
                let prev = layout.cells[(r - 1) * layout.grid_cols];
                let y_top_down = prev.y_top + prev.h;
                let y_b = h_total - y_top_down - layout.gutter;
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(grid_left, y_b),
                    &CGSize::new(avail_w_grid, layout.gutter),
                ));
            }
        }
        ctx
    }

    /// Render `view` into `rect` — fills the rect's terminal background,
    /// draws BG cells / glyphs / underlines / cursor, and (for the
    /// focused session) outlines the cell with a thin focus ring.
    ///
    /// `scratch` holds the per-row decode buffer + per-run glyph slices
    /// — taken from `self.scratch` by the caller so the per-cell loop
    /// here can still call `&mut self` methods (`resolve_char`).
    fn draw_session_in_rect(
        &mut self,
        ctx: &CGContext,
        total_h: u32,
        rect: &CellRect,
        view: &SessionView,
        scratch: &mut Scratch,
    ) {
        // Top of the rect in CG's y-up coords (y=0 is the bottom of
        // the bitmap context).  All per-row baselines are computed
        // off this anchor.
        let rect_top_y_up = total_h as f64 - rect.y_top;

        // Cell BG: BG_FOCUSED (deeper) for the active pane,
        // BG_PANEL for the rest.  The DROP into deeper black is
        // the focus indicator — matches `render_metal::push_session`.
        let pane_bg = if view.focused && self.window_focused {
            BG_FOCUSED
        } else {
            BG_PANEL
        };
        ctx.set_rgb_fill_color(pane_bg.0, pane_bg.1, pane_bg.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(rect.x, rect_top_y_up - rect.h),
            &CGSize::new(rect.w, rect.h),
        ));

        // Apple's default font rendering hints — turn ON everything so
        // CoreText produces the same output it would in Terminal.app.
        ctx.set_allows_antialiasing(true);
        ctx.set_should_antialias(true);
        ctx.set_allows_font_smoothing(true);
        ctx.set_should_smooth_fonts(true);
        ctx.set_allows_font_subpixel_positioning(true);
        ctx.set_should_subpixel_position_fonts(true);
        ctx.set_allows_font_subpixel_quantization(true);
        ctx.set_should_subpixel_quantize_fonts(true);

        ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);

        let grid = view.grid;
        let cols = grid.cols() as usize;

        for r in 0..grid.rows() {
            // Decode the row once: cells + resolved (fg, bg).  All three
            // passes below read from these instead of re-querying the
            // grid + re-resolving SGR per cell per pass.
            scratch.row_cells.clear();
            scratch.row_attrs.clear();
            for c in 0..cols {
                let cell = grid.cell_at_view(view.view_offset, c as u16, r);
                scratch.row_cells.push(cell);
                scratch.row_attrs.push(resolve_attrs(cell.attrs));
            }

            // 1) Background pass — fill runs of cells that share a non-default
            //    background color.  Cells with the default BG inherit the
            //    rect's terminal-bg fill we just laid down.
            let row_bottom_y = rect_top_y_up - (r as f64 + 1.0) * self.font.cell_h;
            let mut c = 0usize;
            while c < cols {
                let bg = scratch.row_attrs[c].1;
                if bg == BG {
                    c += 1;
                    continue;
                }
                let start = c;
                c += 1;
                while c < cols && scratch.row_attrs[c].1 == bg {
                    c += 1;
                }
                ctx.set_rgb_fill_color(bg.0, bg.1, bg.2, 1.0);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(rect.x + start as f64 * self.font.cell_w, row_bottom_y),
                    &CGSize::new((c - start) as f64 * self.font.cell_w, self.font.cell_h),
                ));
            }

            // 2) Foreground pass — group consecutive non-blank cells that
            //    share both font and fg color; one draw_glyphs call per run.
            //    Box-drawing chars (U+2500..) break runs and are painted
            //    by `draw_box_drawing` as cell-sized rect arms — most
            //    monospace fonts ship short `─`/`│` glyphs that don't
            //    span the cell, leaving visible gaps where TUIs draw
            //    box borders (claudecode welcome panel).
            let baseline_y = rect_top_y_up - (r as f64 * self.font.cell_h + self.font.ascent);
            let mut i = 0usize;
            while i < cols {
                let cell = scratch.row_cells[i];
                if cell.ch == ' ' || cell.ch == '\0' {
                    i += 1;
                    continue;
                }
                if box_drawing_arms(cell.ch).is_some() || block_element_rects(cell.ch).is_some() {
                    // Per-glyph mask path: rasterise the box/block
                    // character into a cell-sized grayscale mask once
                    // (cached), then blit at the integer cell rect with
                    // the current fg colour. Composing arms in
                    // cell-local coords removes every dependency on
                    // the cell's absolute screen origin, so corners
                    // join pixel-perfectly regardless of cumulative
                    // float drift in `i * cell_w`.
                    let fg = scratch.row_attrs[i].0;
                    let xl_unsnap = rect.x + i as f64 * self.font.cell_w;
                    let xr_int = (xl_unsnap + self.font.cell_w).round();
                    let xl_int = xl_unsnap.round();
                    let yb_int = row_bottom_y.round();
                    let yt_int = (row_bottom_y + self.font.cell_h).round();
                    let w_int = (xr_int - xl_int).max(1.0) as u16;
                    let h_int = (yt_int - yb_int).max(1.0) as u16;
                    let mask = get_or_rasterize_box_mask(
                        &mut self.box_mask_cache,
                        cell.ch,
                        w_int,
                        h_int,
                    );
                    let cell_rect = CGRect::new(
                        &CGPoint::new(xl_int, yb_int),
                        &CGSize::new(w_int as f64, h_int as f64),
                    );
                    ctx.save();
                    // CRITICAL: blit-time AA and interpolation must BOTH
                    // be off. Without these, CG resamples the mask edges
                    // and the corners go soft — the exact "拐角对不齐"
                    // symptom. Geometry-perfect masks blitted through
                    // the default (AA on, interpolation default) state
                    // come out as if drawn with sub-pixel arms.
                    ctx.set_should_antialias(false);
                    ctx.set_interpolation_quality(CGInterpolationQuality::CGInterpolationQualityNone);
                    ctx.clip_to_mask(cell_rect, mask);
                    ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                    ctx.fill_rect(cell_rect);
                    ctx.restore();
                    i += 1;
                    continue;
                }
                let (font_idx, glyph) =
                    self.font.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
                let fg = scratch.row_attrs[i].0;
                scratch.run_glyphs.clear();
                scratch.run_positions.clear();
                scratch.run_glyphs.push(glyph);
                scratch
                    .run_positions
                    .push(CGPoint::new(rect.x + i as f64 * self.font.cell_w, baseline_y));
                i += 1;
                while i < cols {
                    let cur = scratch.row_cells[i];
                    if cur.ch == ' ' || cur.ch == '\0' {
                        break;
                    }
                    if box_drawing_arms(cur.ch).is_some()
                        || block_element_rects(cur.ch).is_some()
                    {
                        break;
                    }
                    let (cur_font, cur_glyph) =
                        self.font.resolve_char(cur.ch, cur.attrs.bold, cur.attrs.italic);
                    if cur_font != font_idx || scratch.row_attrs[i].0 != fg {
                        break;
                    }
                    scratch.run_glyphs.push(cur_glyph);
                    scratch
                        .run_positions
                        .push(CGPoint::new(rect.x + i as f64 * self.font.cell_w, baseline_y));
                    i += 1;
                }
                ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                let font = &self.font.font(font_idx);
                font.draw_glyphs(&scratch.run_glyphs, &scratch.run_positions, ctx.clone());
            }

            // 3) Underline pass.
            let underline_y = row_bottom_y + (self.font.cell_h - self.font.ascent) * 0.55;
            let underline_h = (self.font.cell_h * 0.06).max(1.0);
            let mut u = 0usize;
            while u < cols {
                let cell = scratch.row_cells[u];
                if !cell.attrs.underline {
                    u += 1;
                    continue;
                }
                let fg = scratch.row_attrs[u].0;
                let start = u;
                u += 1;
                while u < cols {
                    let cur = scratch.row_cells[u];
                    if !cur.attrs.underline || scratch.row_attrs[u].0 != fg {
                        break;
                    }
                    u += 1;
                }
                ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(rect.x + start as f64 * self.font.cell_w, underline_y),
                    &CGSize::new((u - start) as f64 * self.font.cell_w, underline_h),
                ));
            }
        }

        // Cursor: only when this session is in live view + DECTCEM is on.
        if view.view_offset == 0 && view.cursor_visible {
            self.draw_cursor_in_rect(ctx, rect_top_y_up, rect, view);
        }

        // Focus indicator on the AppKit fallback path is just the
        // pane-darken overlay (handled implicitly by the Metal path's
        // BG-pipeline overlay; AppKit doesn't have a layered alpha
        // pass yet).  The "focus frame is the gutter" treatment in
        // render_metal.rs::push_session needs equivalent gutter-aware
        // drawing here when AppKit comes back into rotation.
    }

    /// Sidebar pass: one row per session.  Row N: [focus highlight bg,]
    /// state dot, label.  Caller is responsible for matching the row
    /// height & top padding constants when hit-testing clicks
    /// (`SIDEBAR_TOP_PAD`, `SIDEBAR_ROW_H`).
    fn draw_sidebar(
        &mut self,
        ctx: &CGContext,
        total_h: u32,
        layout: &Layout,
        entries: &[SidebarEntry],
        focused_idx: usize,
    ) {
        for (i, entry) in entries.iter().enumerate() {
            let row_top_y_down = layout.top_inset
                + layout.sidebar_top_pad_phys
                + i as f64 * SIDEBAR_ROW_H;
            let row_top_y_up = total_h as f64 - row_top_y_down;
            let row_bottom_y = row_top_y_up - SIDEBAR_ROW_H;

            // Focus highlight: subtle blue-grey fill behind the focused row.
            if i == focused_idx {
                ctx.set_rgb_fill_color(
                    SIDEBAR_FOCUSED_BG.0,
                    SIDEBAR_FOCUSED_BG.1,
                    SIDEBAR_FOCUSED_BG.2,
                    1.0,
                );
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(0.0, row_bottom_y),
                    &CGSize::new(layout.sidebar_w, SIDEBAR_ROW_H),
                ));
            }

            // State dot.  Vertically-centred in the row.
            let dot_color = match entry.state {
                SessionState::Active => STATE_ACTIVE,
                SessionState::Idle => STATE_IDLE,
                SessionState::Exited => STATE_EXITED,
            };
            let dot_cx = SIDEBAR_LEFT_PAD + SIDEBAR_DOT_R;
            let dot_cy = row_top_y_up - SIDEBAR_ROW_H / 2.0;
            ctx.set_rgb_fill_color(dot_color.0, dot_color.1, dot_color.2, 1.0);
            ctx.fill_ellipse_in_rect(CGRect::new(
                &CGPoint::new(dot_cx - SIDEBAR_DOT_R, dot_cy - SIDEBAR_DOT_R),
                &CGSize::new(SIDEBAR_DOT_R * 2.0, SIDEBAR_DOT_R * 2.0),
            ));

            // Label text.  Reuse the base font; lay out via cell_w
            // monospace metrics — sidebar chars are typically 1–8
            // ASCII so cell_w accuracy is fine.
            let label_x = dot_cx + SIDEBAR_DOT_R + SIDEBAR_DOT_LABEL_GAP;
            // See render_metal.rs sidebar baseline note: align an ASCII
            // digit's visual centre with the dot centre.  In y-up,
            // baseline sits ~0.30 × ascent BELOW the dot centre.
            let baseline_y =
                row_top_y_up - SIDEBAR_ROW_H / 2.0 - self.font.ascent * 0.30;
            ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            ctx.set_rgb_fill_color(
                SIDEBAR_TEXT_FG.0,
                SIDEBAR_TEXT_FG.1,
                SIDEBAR_TEXT_FG.2,
                1.0,
            );
            let mut glyphs: Vec<CGGlyph> = Vec::with_capacity(entry.label.len());
            let mut positions: Vec<CGPoint> = Vec::with_capacity(entry.label.len());
            let mut x = label_x;
            for ch in entry.label.chars() {
                let (font_idx, g) = self.font.resolve_char(ch, false, false);
                if g != 0 && font_idx == 0 {
                    glyphs.push(g);
                    positions.push(CGPoint::new(x, baseline_y));
                }
                x += self.font.cell_w;
            }
            if !glyphs.is_empty() {
                let font = &self.font.font(0);
                font.draw_glyphs(&glyphs, &positions, ctx.clone());
            }
        }
    }

    /// Cursor: filled block when this session is focused AND the window
    /// is focused; hollow outline otherwise.  For the filled case we
    /// re-draw the cell's glyph in the background colour so the
    /// character stays readable.
    fn draw_cursor_in_rect(
        &mut self,
        ctx: &CGContext,
        rect_top_y_up: f64,
        rect: &CellRect,
        view: &SessionView,
    ) {
        let (col, row) = view.grid.cursor();
        let cx = rect.x + col as f64 * self.font.cell_w;
        let cy_bottom = rect_top_y_up - (row as f64 + 1.0) * self.font.cell_h;
        let cursor_rect = CGRect::new(
            &CGPoint::new(cx, cy_bottom),
            &CGSize::new(self.font.cell_w, self.font.cell_h),
        );
        let solid = view.focused && self.window_focused;

        if solid {
            ctx.set_rgb_fill_color(FG.0, FG.1, FG.2, 1.0);
            ctx.fill_rect(cursor_rect);

            let cell = view.grid.cell(col, row);
            if cell.ch != ' ' && cell.ch != '\0' {
                let (font_idx, glyph) =
                    self.font.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
                if glyph != 0 {
                    ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
                    let baseline_y =
                        rect_top_y_up - (row as f64 * self.font.cell_h + self.font.ascent);
                    let font = &self.font.font(font_idx);
                    font.draw_glyphs(
                        &[glyph],
                        &[CGPoint::new(cx, baseline_y)],
                        ctx.clone(),
                    );
                }
            }
        } else {
            // Hollow outline.
            let stroke = (self.font.cell_h * 0.07).max(1.0);
            ctx.set_rgb_stroke_color(FG.0, FG.1, FG.2, 1.0);
            ctx.set_line_width(stroke);
            ctx.stroke_rect(cursor_rect);
        }
    }
}

/// Box-drawing arms encoded as N|S|W|E bits (4 LSBs).
const ARM_N: u8 = 0b0001;
const ARM_S: u8 = 0b0010;
const ARM_W: u8 = 0b0100;
const ARM_E: u8 = 0b1000;

/// If `ch` is a box-drawing character we paint programmatically, return
/// its arm composition.  Covers the common subset of U+2500..U+257F
/// that TUIs (claudecode welcome panel, htop, ncurses dialogs) use to
/// draw frames — single-line corners, tees, crosses.  Heavy / double
/// variants intentionally fall back to the font for now.
///
/// The font's `─`, `│` glyphs are usually shorter than the cell advance,
/// so painting them via `draw_glyphs` leaves visible gaps between
/// adjacent box cells; this table lets the renderer composite arms that
/// extend slightly past the cell midline so neighbours always join.
fn box_drawing_arms(ch: char) -> Option<u8> {
    Some(match ch {
        '\u{2500}' | '\u{2501}' => ARM_W | ARM_E,         // ─ ━
        '\u{2502}' | '\u{2503}' => ARM_N | ARM_S,         // │ ┃
        '\u{250C}' | '\u{250D}' | '\u{250E}' | '\u{250F}' => ARM_E | ARM_S, // ┌ variants
        '\u{2510}' | '\u{2511}' | '\u{2512}' | '\u{2513}' => ARM_W | ARM_S, // ┐ variants
        '\u{2514}' | '\u{2515}' | '\u{2516}' | '\u{2517}' => ARM_E | ARM_N, // └ variants
        '\u{2518}' | '\u{2519}' | '\u{251A}' | '\u{251B}' => ARM_W | ARM_N, // ┘ variants
        '\u{251C}'..='\u{2523}' => ARM_N | ARM_S | ARM_E, // ├ variants
        '\u{2524}'..='\u{252B}' => ARM_N | ARM_S | ARM_W, // ┤ variants
        '\u{252C}'..='\u{2533}' => ARM_W | ARM_E | ARM_S, // ┬ variants
        '\u{2534}'..='\u{253B}' => ARM_W | ARM_E | ARM_N, // ┴ variants
        '\u{253C}'..='\u{254B}' => ARM_W | ARM_E | ARM_N | ARM_S, // ┼ variants
        // Rounded corners — visually identical join behaviour to ┌┐└┘,
        // just with the corner pixel filled (our integer-pixel rect
        // fill IS a square corner; "rounded" font glyphs would only
        // differ at sub-pixel scale, which we don't have). Adding
        // them here was the load-bearing fix for claudecode's welcome
        // box, which uses U+256D-U+2570 not U+250C-U+2518.
        '\u{256D}' => ARM_E | ARM_S, // ╭ rounded top-left  ≡ ┌
        '\u{256E}' => ARM_W | ARM_S, // ╮ rounded top-right ≡ ┐
        '\u{256F}' => ARM_W | ARM_N, // ╯ rounded bot-right ≡ ┘
        '\u{2570}' => ARM_E | ARM_N, // ╰ rounded bot-left  ≡ └
        _ => return None,
    })
}

/// Block-element layout: a pair of fractions describing the filled
/// region inside the cell.  Eighths are encoded as eighths-of-cell
/// (0..=8) so we round per-cell once and avoid drift across columns.
///
/// `BlockRect::Eighths { x_left_8, y_bot_8, x_right_8, y_top_8 }` —
/// the rect spans `x_left_8/8 .. x_right_8/8` of the cell width and
/// `y_bot_8/8 .. y_top_8/8` of the cell height. Two rects allow
/// quadrant glyphs (U+2596..U+259F) where the filled area is L-shaped.
#[derive(Clone, Copy)]
struct BlockRect {
    x_left_8: u8,
    y_bot_8: u8,
    x_right_8: u8,
    y_top_8: u8,
}

#[derive(Clone, Copy)]
struct BlockShape {
    rects: [Option<BlockRect>; 2],
    /// Alpha for shaded variants ░ ▒ ▓; opaque for the rest.
    alpha: f64,
}

const fn r(x_left_8: u8, y_bot_8: u8, x_right_8: u8, y_top_8: u8) -> Option<BlockRect> {
    Some(BlockRect { x_left_8, y_bot_8, x_right_8, y_top_8 })
}

const fn one(rect: Option<BlockRect>) -> BlockShape {
    BlockShape { rects: [rect, None], alpha: 1.0 }
}

const fn two(a: Option<BlockRect>, b: Option<BlockRect>) -> BlockShape {
    BlockShape { rects: [a, b], alpha: 1.0 }
}

const fn shaded(alpha: f64) -> BlockShape {
    BlockShape { rects: [r(0, 0, 8, 8), None], alpha }
}

/// U+2580..U+259F block elements (lower/upper N/8, side N/8, quadrants,
/// shaded). Used by claudecode for the pixel-art welcome icon and by
/// progress bars, sparklines, etc. The font's glyphs for these don't
/// span the cell so we paint them directly like box-drawing chars.
fn block_element_rects(ch: char) -> Option<BlockShape> {
    Some(match ch {
        '\u{2580}' => one(r(0, 4, 8, 8)),    // ▀ upper half
        '\u{2581}' => one(r(0, 0, 8, 1)),    // ▁ lower 1/8
        '\u{2582}' => one(r(0, 0, 8, 2)),    // ▂ lower 2/8
        '\u{2583}' => one(r(0, 0, 8, 3)),    // ▃
        '\u{2584}' => one(r(0, 0, 8, 4)),    // ▄ lower half
        '\u{2585}' => one(r(0, 0, 8, 5)),    // ▅
        '\u{2586}' => one(r(0, 0, 8, 6)),    // ▆
        '\u{2587}' => one(r(0, 0, 8, 7)),    // ▇
        '\u{2588}' => one(r(0, 0, 8, 8)),    // █ full
        '\u{2589}' => one(r(0, 0, 7, 8)),    // ▉ left 7/8
        '\u{258A}' => one(r(0, 0, 6, 8)),    // ▊
        '\u{258B}' => one(r(0, 0, 5, 8)),    // ▋
        '\u{258C}' => one(r(0, 0, 4, 8)),    // ▌ left half
        '\u{258D}' => one(r(0, 0, 3, 8)),    // ▍
        '\u{258E}' => one(r(0, 0, 2, 8)),    // ▎
        '\u{258F}' => one(r(0, 0, 1, 8)),    // ▏
        '\u{2590}' => one(r(4, 0, 8, 8)),    // ▐ right half
        '\u{2591}' => shaded(0.25),          // ░ light shade
        '\u{2592}' => shaded(0.50),          // ▒ medium shade
        '\u{2593}' => shaded(0.75),          // ▓ dark shade
        '\u{2594}' => one(r(0, 7, 8, 8)),    // ▔ upper 1/8
        '\u{2595}' => one(r(7, 0, 8, 8)),    // ▕ right 1/8
        '\u{2596}' => one(r(0, 0, 4, 4)),    // ▖ lower-left quadrant
        '\u{2597}' => one(r(4, 0, 8, 4)),    // ▗ lower-right
        '\u{2598}' => one(r(0, 4, 4, 8)),    // ▘ upper-left
        '\u{2599}' => two(r(0, 4, 4, 8), r(0, 0, 8, 4)), // ▙ UL + lower half
        '\u{259A}' => two(r(0, 4, 4, 8), r(4, 0, 8, 4)), // ▚ UL + LR
        '\u{259B}' => two(r(0, 4, 8, 8), r(0, 0, 4, 4)), // ▛ upper half + LL
        '\u{259C}' => two(r(0, 4, 8, 8), r(4, 0, 8, 4)), // ▜ upper half + LR
        '\u{259D}' => one(r(4, 4, 8, 8)),    // ▝ upper-right
        '\u{259E}' => two(r(4, 4, 8, 8), r(0, 0, 4, 4)), // ▞ UR + LL
        '\u{259F}' => two(r(4, 4, 8, 8), r(0, 0, 8, 4)), // ▟ UR + lower half
        _ => return None,
    })
}


/// Look up (or rasterise on miss) the grayscale mask for one box/block
/// character at the given cell pixel size. The cache key intentionally
/// excludes colour — colour comes from the destination context's fill
/// state at blit time, applied via `clip_to_mask`. Variable cell widths
/// (8 vs 9 px on common monospace × 2× scale) get separate entries so
/// neighbouring columns of different physical width still produce
/// identical-looking arms.
fn get_or_rasterize_box_mask<'a>(
    cache: &'a mut std::collections::HashMap<(u32, u16, u16), CGImage>,
    ch: char,
    w: u16,
    h: u16,
) -> &'a CGImage {
    let key = (ch as u32, w, h);
    cache
        .entry(key)
        .or_insert_with(|| rasterize_box_mask(ch, w, h))
}

/// Build a `w × h` grayscale CGImage whose white pixels are the
/// character's arms and black pixels are the empty cell area. The
/// arms compose in cell-local coords starting from (0, 0); this is
/// the load-bearing property — without absolute screen origin in the
/// math, two adjacent cells produce arms that fit together with zero
/// drift regardless of their absolute grid position.
fn rasterize_box_mask(ch: char, w: u16, h: u16) -> CGImage {
    // Direct byte-buffer rasterisation — kitty's approach. Bypassing
    // CGContext drawing primitives is intentional: CG's fill_rect adds
    // sub-pixel rounding even with AA off, and the round/floor mismatch
    // between `xm = round(x_left + cell_w/2)` and `mid = w/2` (integer
    // division) produced visible 1-px corner drift between cells of
    // width 8 and 9.  Writing pixels directly uses kitty's exact
    // formula: `mid = w/2`, `stroke run = [mid - t/2, mid - t/2 + t)`.
    let w_u = w as usize;
    let h_u = h as usize;
    let mut buf = vec![0u8; w_u * h_u];

    if let Some(arms) = box_drawing_arms(ch) {
        rasterize_arms_into_buf(&mut buf, w_u, h_u, arms);
    } else if let Some(shape) = block_element_rects(ch) {
        rasterize_block_into_buf(&mut buf, w_u, h_u, shape);
    }

    let buf_arc = std::sync::Arc::new(buf);
    let provider = core_graphics::data_provider::CGDataProvider::from_buffer(buf_arc);
    let space = CGColorSpace::create_device_gray();
    CGImage::new(
        w_u,
        h_u,
        8,                 // bits per component
        8,                 // bits per pixel
        w_u,               // bytes per row (1 byte per pixel)
        &space,
        kCGImageAlphaNone, // grayscale, no alpha — pixel value IS the mask coverage
        &provider,
        false,             // no interpolation on this image
        0,                 // CGColorRenderingIntent default (kCGRenderingIntentDefault)
    )
}

/// Write box-drawing arms directly into a w×h grayscale byte buffer in
/// y-down image-natural orientation (row 0 = top). Uses kitty's exact
/// formula: integer midline + stroke run `[mid - t/2, mid - t/2 + t)`.
/// Each arm extends past the centerline by `half_t_hi` into the
/// perpendicular arm's column so the corner overlap region is fully
/// covered with no notch.
fn rasterize_arms_into_buf(buf: &mut [u8], w: usize, h: usize, arms: u8) {
    // Stroke thickness — kitty/alacritty's `max(1, round(cell_w/8))`.
    let t = (((w as f32) / 8.0).round() as i32).max(1) as usize;
    let half_t_lo = t / 2;          // integer floor
    let half_t_hi = t - half_t_lo;  // integer ceil — equals half_t_lo for even t, +1 for odd
    let mid_x = w / 2;              // integer midline; consistent across cells of any width
    let mid_y = h / 2;
    // Stroke runs are exactly `t` rows / columns wide (kitty's
    // `start + stroke`, never re-derived from `center ± t/2` → no
    // off-by-one between even / odd `t` and even / odd cell dims).
    let stroke_x0 = mid_x.saturating_sub(half_t_lo);
    let stroke_y0 = mid_y.saturating_sub(half_t_lo);
    let stroke_x1 = (stroke_x0 + t).min(w);
    let stroke_y1 = (stroke_y0 + t).min(h);

    let fill = |buf: &mut [u8], x0: usize, x1: usize, y0: usize, y1: usize| {
        let x0 = x0.min(w);
        let x1 = x1.min(w);
        let y0 = y0.min(h);
        let y1 = y1.min(h);
        for y in y0..y1 {
            let row_start = y * w;
            buf[row_start + x0..row_start + x1].fill(0xff);
        }
    };

    // In image y-down: ARM_N = top of cell (low y), ARM_S = bottom (high y).
    // Each arm extends past midline by `half_t_hi` into the perpendicular
    // arm's column — this is what fills the corner pocket.
    if arms & ARM_W != 0 {
        fill(buf, 0, mid_x + half_t_hi, stroke_y0, stroke_y1);
    }
    if arms & ARM_E != 0 {
        fill(buf, stroke_x0, w, stroke_y0, stroke_y1);
    }
    if arms & ARM_N != 0 {
        fill(buf, stroke_x0, stroke_x1, 0, mid_y + half_t_hi);
    }
    if arms & ARM_S != 0 {
        fill(buf, stroke_x0, stroke_x1, stroke_y0, h);
    }
}

/// Write block-element shape directly into a w×h grayscale byte buffer
/// in y-down image orientation. BlockRect coords are in CG y-up eighths
/// (the same units the `block_element_rects` table uses for the mask
/// approach), so we flip the y component when computing image rows.
fn rasterize_block_into_buf(buf: &mut [u8], w: usize, h: usize, shape: BlockShape) {
    let fill_val = if shape.alpha < 1.0 {
        (255.0 * shape.alpha) as u8
    } else {
        0xff
    };
    for opt_r in shape.rects.iter() {
        let Some(r) = opt_r else { continue };
        let x0 = (w * r.x_left_8 as usize) / 8;
        let x1 = (w * r.x_right_8 as usize) / 8;
        // BlockRect is in CG y-up eighths: y_bot_8 = bottom in CG = SCREEN bottom
        // = HIGH image y. y_top_8 = top in CG = SCREEN top = LOW image y.
        let y0_img = (h * (8 - r.y_top_8 as usize)) / 8;
        let y1_img = (h * (8 - r.y_bot_8 as usize)) / 8;
        let x0 = x0.min(w);
        let x1 = x1.min(w);
        let y0 = y0_img.min(h);
        let y1 = y1_img.min(h);
        for y in y0..y1 {
            let row_start = y * w;
            buf[row_start + x0..row_start + x1].fill(fill_val);
        }
    }
}
