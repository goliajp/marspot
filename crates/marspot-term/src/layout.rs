//! Window layout: where each session and the sidebar live, in
//! physical pixels.
//!
//! marspot carves the window into:
//!
//! ```text
//!   ┌─────────┬───────────────────────────┐
//!   │ sidebar │  N×M grid of session cells │
//!   │         │  ┌──────┬──────┬──────┐   │
//!   │         │  │ cell │ cell │ cell │   │
//!   │         │  ├──────┼──────┼──────┤   │
//!   │         │  │ cell │ cell │ cell │   │
//!   │         │  └──────┴──────┴──────┘   │
//!   └─────────┴───────────────────────────┘
//! ```
//!
//! Sizes are everywhere in **physical pixels** so the renderer can
//! consume them directly.  The caller passes `cell_w` / `cell_h`
//! (font cell metrics) so each session knows how many terminal
//! columns / rows actually fit in its physical sub-rect.

/// A simple physical-pixel rectangle.  Used for non-cell UI chrome
/// elements (layout-picker button, picker option icons, sidebar
/// close-buttons, sidebar add-button).  Distinct from `CellRect`
/// because those carry terminal cols/rows; these are pure UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y_top: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y_top && py < self.y_top + self.h
    }
    pub const ZERO: Rect = Rect { x: 0.0, y_top: 0.0, w: 0.0, h: 0.0 };
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellRect {
    /// Physical-pixel x of the rect's left edge.
    pub x: f64,
    /// Physical-pixel y of the rect's TOP edge (y-down).  The renderer
    /// flips to y-up when computing baselines.
    pub y_top: f64,
    pub w: f64,
    pub h: f64,
    /// Terminal column count that fits in this rect at the configured
    /// font cell width.
    pub cols: u16,
    /// Terminal row count that fits.
    pub rows: u16,
}

#[derive(Clone, Debug)]
pub struct Layout {
    /// Total window dimensions in physical pixels.
    pub window_w: f64,
    pub window_h: f64,
    /// Sidebar width in physical pixels.  The sidebar occupies
    /// `[0, sidebar_w] × [top_inset, window_h]`.  Set to 0 for no
    /// sidebar.
    pub sidebar_w: f64,
    /// Top inset in physical pixels — a clear band above sidebar
    /// items AND cells, reserved for the macOS traffic-light buttons
    /// when `FullSizeContentView` is on.  Without it, the buttons
    /// would overlap whatever the renderer paints into the top-left
    /// (sidebar item 1, cell 1's first row).  Set to 0 for headless
    /// / snapshot use where there's no window chrome to clear.
    pub top_inset: f64,
    /// `(cols, rows)` of the session grid layout itself (not the
    /// terminal cell grid inside one session — that's per-cell).
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// The N session sub-rects, in row-major order.  Length is
    /// `grid_cols * grid_rows`.
    pub cells: Vec<CellRect>,
    /// Inter-cell gutter width in physical pixels.  The renderer
    /// uses this to paint the focus indicator AS the gutter around
    /// the focused cell (focus frame and divider are the same
    /// thing — no separate inner stroke).  0 if the grid is 1×1.
    pub gutter: f64,
    /// Inner padding on each side of every cell, in physical pixels.
    /// `cell.cols` / `cell.rows` are counted off the area AFTER
    /// padding; the renderer offsets glyph origins by this amount
    /// so terminal content doesn't crowd the cell's visible edge.
    pub padding: f64,
    /// Title strip height at the top of every cell, in physical
    /// pixels.  The renderer paints the strip with the session
    /// label + a SEAM hairline at its bottom; terminal content
    /// (cols × rows × glyphs) starts BELOW this strip.  Set to 0
    /// for headless / single-pane snapshot layouts that don't
    /// want a title band.
    pub cell_title_h: f64,
    /// Floating [layout] button — top-right of the main session-
    /// grid area.  Renderer paints a small rounded chip showing the
    /// current grid shape; clicking toggles the picker.  Always
    /// present (even at 1×1) so the user can switch layouts.
    pub layout_button_rect: Rect,
    /// Floating [sidebar] button — sits immediately left of the
    /// layout button.  Clicking toggles the sidebar collapsed state.
    /// Always present (even when the sidebar is currently collapsed)
    /// so the user can re-open it.
    pub sidebar_button_rect: Rect,
    /// F3+1 — third chrome icon button immediately right of the
    /// layout button.  Toggles the right-side process-tree panel.
    /// Always present so the user can pop the panel any time.
    pub process_button_rect: Rect,
    /// Layout-picker overlay panel — `Some` while the picker is
    /// showing, `None` otherwise.  Renderer paints the panel BG
    /// behind the option icons; mouse_down hits inside this rect
    /// (but outside any option) close the picker.
    pub picker_panel_rect: Option<Rect>,
    /// Per-option icon rect inside the picker, in render order.
    /// Length matches `picker_option_dims`.  Empty when the picker
    /// is closed.
    pub picker_option_rects: Vec<Rect>,
    /// `(grid_cols, grid_rows)` for each picker option, parallel to
    /// `picker_option_rects`.  Renderer reads this to draw the
    /// preview grid inside each option icon.  Same indices as the
    /// `LayoutMode` enum variants (Single, SplitH, SplitV, Quad,
    /// SixH, SixV, Nine).
    pub picker_option_dims: Vec<(usize, usize)>,
    /// One rect per sidebar row — the close [×] hit-target on the
    /// row's right edge.  Length == n_sessions.  Empty when the
    /// chrome wasn't built or n_sessions == 0.  Renderer paints a
    /// [×] glyph or hairline cross inside each rect.  The first
    /// (only) rect is the "last session" — `mouse_down` may choose
    /// to ignore clicks on it to prevent killing the final session.
    pub close_session_rects: Vec<Rect>,
    /// [+] add-session button — a row-tall band immediately under
    /// the macOS header strip, above sidebar row 0.  Always present
    /// when the sidebar is shown (renderer paints it disabled when
    /// `n_sessions >= 9`).  Width spans the sidebar minus padding.
    pub add_session_button_rect: Rect,
    /// Y offset (physical px) from `top_inset` to the top of
    /// sidebar row 0.  Reserves space for the header gap + add-
    /// session button + body gap when the sidebar is shown; zero
    /// when sidebar_w == 0.  Both renderers (Metal / AppKit) read
    /// this instead of a hard-coded constant so the row positions
    /// + chrome rects stay in lockstep.
    pub sidebar_top_pad_phys: f64,
}

/// Layouts the picker offers, in the order they appear in the
/// overlay (left → right).  Kept here so layout.rs can compute
/// rects without depending on main.rs's LayoutMode enum.
const PICKER_LAYOUT_DIMS: [(usize, usize); 7] = [
    (1, 1), // Single
    (2, 1), // SplitH (horizontal split, cells side-by-side)
    (1, 2), // SplitV (vertical split, cells stacked)
    (2, 2), // Quad
    (3, 2), // SixH
    (2, 3), // SixV
    (3, 3), // Nine
];

/// Square icon-button side length (logical pt). Both chrome buttons
/// (sidebar toggle, layout picker) are now true squares — Lucide-
/// style monochrome line icons sit inside a 22×22 hit-target.
const ICON_BUTTON_LOGICAL_SIZE: f64 = 22.0;
const ICON_BUTTON_LOGICAL_MARGIN: f64 = 8.0;
const ICON_BUTTON_LOGICAL_GAP: f64 = 6.0;

/// Picker option icon size + spacing.  7 options × 28 + 6 × 4 + 2 × 8 = 220 logical pt wide.
const PICKER_OPTION_LOGICAL_SIZE: f64 = 28.0;
const PICKER_OPTION_LOGICAL_GAP: f64 = 4.0;
const PICKER_PANEL_LOGICAL_PAD: f64 = 8.0;
const PICKER_PANEL_LOGICAL_GAP_FROM_BUTTON: f64 = 6.0;

/// Sidebar geometry, all in physical pixels.  Renderer reads
/// `Layout::sidebar_top_pad_phys` (computed at build time so the
/// header band reserved for the [+] add-session button is accounted
/// for); session-row height stays a fixed phys constant.
pub const SIDEBAR_ROW_H_PHYS: f64 = 22.0;
const SIDEBAR_HEADER_GAP_PHYS: f64 = 8.0;
const SIDEBAR_ADD_BTN_H_PHYS: f64 = 22.0;
const SIDEBAR_BODY_GAP_PHYS: f64 = 8.0;
const SIDEBAR_ADD_BTN_X_PAD_PHYS: f64 = 8.0;
const SIDEBAR_CLOSE_PHYS_SIZE: f64 = 14.0;
const SIDEBAR_CLOSE_PHYS_MARGIN_RIGHT: f64 = 8.0;

impl Layout {
    /// Build a layout for `grid_cols × grid_rows` sessions inside a
    /// window of `(window_w, window_h)` physical pixels, leaving a
    /// `sidebar_w`-wide strip on the left for chrome.
    ///
    /// `cell_w` / `cell_h` are the font's per-character cell metrics
    /// — used to compute how many terminal cols / rows fit in each
    /// sub-rect.
    pub fn build(
        window_w: f64,
        window_h: f64,
        sidebar_w: f64,
        top_inset: f64,
        cell_title_h: f64,
        grid_cols: usize,
        grid_rows: usize,
        cell_w: f64,
        cell_h: f64,
    ) -> Self {
        assert!(grid_cols > 0 && grid_rows > 0);
        // When a sidebar is present, reserve one 1 px strip between
        // it and the 9-grid for the SEAM hairline.  Same physical
        // width as inter-cell seams — iTerm2 reads as a 1-pixel
        // hairline; anything wider looks like chrome.
        let sidebar_seam = if sidebar_w > 0.0 { 1.0 } else { 0.0 };
        let avail_w = (window_w - sidebar_w - sidebar_seam).max(0.0);
        let avail_h = (window_h - top_inset).max(0.0);
        // Inter-cell gutter in physical pixels — a thin strip of
        // chrome (the GUTTER color cleared by the renderer) shows
        // between sessions so the 3×3 grid reads as a grid, not as a
        // single sea of identical-looking shells.  2 px = 1 logical
        // point at 2× retina; iTerm2-style hairline.  Skipped when
        // the grid is 1×1 (single session, nothing to divide).
        let gutter = if grid_cols > 1 || grid_rows > 1 { 1.0 } else { 0.0 };
        // Inner padding (physical px) — breathing room between the
        // cell rect's edge and the first/last terminal column / row.
        // Without this, "Last login: ..." crowds the very top-left
        // pixel of the cell.  ~8 px ≈ 4 logical points at 2× retina,
        // matches iTerm2's default leftmargin/topmargin feel.
        let padding = 8.0;

        // Cells flush to the available area's outer edges — gutters
        // appear ONLY between cells, never around the outside.  Old
        // layout left a `gutter/2` margin on each side; that strip
        // showed the renderer's GUTTER clear colour (a light grey
        // hairline) all the way around the 9-grid, framing it like
        // a chrome inset.  Worse, the strip extended into macOS's
        // rounded window corners — the rounded mask clipped the
        // grey strip into a "chipped" look.  Flushing cells means
        // cell BG paints the whole window-content rectangle and
        // the rounding just clips dark-on-dark.
        let inner_w = (avail_w - (grid_cols - 1) as f64 * gutter).max(1.0);
        let inner_h = (avail_h - (grid_rows - 1) as f64 * gutter).max(1.0);
        let cell_phys_w = (inner_w / grid_cols as f64).max(1.0);
        let cell_phys_h = (inner_h / grid_rows as f64).max(1.0);
        // Inner content area (after padding + title strip) is what
        // cols/rows are counted from.  The cell rect itself stays
        // at the outer size — the title strip and BG fill cover
        // the whole cell so the padding zone reads as terminal-bg.
        let cell_inner_w = (cell_phys_w - 2.0 * padding).max(1.0);
        let cell_inner_h =
            (cell_phys_h - cell_title_h - 2.0 * padding).max(1.0);

        let mut cells = Vec::with_capacity(grid_cols * grid_rows);
        for r in 0..grid_rows {
            for c in 0..grid_cols {
                let x = sidebar_w + sidebar_seam + c as f64 * (cell_phys_w + gutter);
                let y_top = top_inset + r as f64 * (cell_phys_h + gutter);
                let cols = ((cell_inner_w / cell_w).floor() as u16).max(1);
                let rows = ((cell_inner_h / cell_h).floor() as u16).max(1);
                cells.push(CellRect {
                    x,
                    y_top,
                    w: cell_phys_w,
                    h: cell_phys_h,
                    cols,
                    rows,
                });
            }
        }
        // Sidebar-top pad: header gap + add-session button + body
        // gap when the sidebar is shown.  Zero otherwise (snapshot /
        // bench paths don't reserve a header band).  Computed here
        // (not in `with_chrome`) because the renderer's row math
        // reads it whether or not chrome was layered on.
        let sidebar_top_pad_phys = if sidebar_w > 0.0 {
            SIDEBAR_HEADER_GAP_PHYS + SIDEBAR_ADD_BTN_H_PHYS + SIDEBAR_BODY_GAP_PHYS
        } else {
            0.0
        };
        Self {
            window_w,
            window_h,
            sidebar_w,
            top_inset,
            grid_cols,
            grid_rows,
            cells,
            gutter,
            padding,
            cell_title_h,
            // Chrome rects default to ZERO / empty — call `with_chrome`
            // to populate them after build.  Snapshot/bench/test
            // call sites that don't render the picker leave them
            // zeroed; marspot's main path always layers on the chrome.
            layout_button_rect: Rect::ZERO,
            sidebar_button_rect: Rect::ZERO,
            process_button_rect: Rect::ZERO,
            picker_panel_rect: None,
            picker_option_rects: Vec::new(),
            picker_option_dims: Vec::new(),
            close_session_rects: Vec::new(),
            add_session_button_rect: Rect::ZERO,
            sidebar_top_pad_phys,
        }
    }

    /// Layer the floating-chrome rects (the [layout] button, the
    /// picker overlay when `picker_open`, and the per-row close [×]
    /// rects) onto an already-built `Layout`.  Done as a post-build
    /// step so snapshot / bench / test paths that don't render
    /// chrome can call the original `build` unchanged.
    /// `scale` is the device pixel ratio (caller already has it
    /// from `ctx.scale()`); we use it for the button + picker
    /// (logical-pt sizing).  `toolbar_top_phys` is the physical-px y
    /// of the toolbar band (0 < toolbar_top_phys < top_inset means
    /// the chrome above the grid is split title-strip + toolbar, and
    /// buttons live in the lower band).  Pass 0.0 to keep buttons in
    /// the legacy single-band layout for snapshot / bench callers.
    /// `n_sessions` populates the close-[×] rects (one per row).
    pub fn with_chrome(
        mut self,
        scale: f64,
        picker_open: bool,
        n_sessions: usize,
        toolbar_top_phys: f64,
    ) -> Self {
        let btn_size = ICON_BUTTON_LOGICAL_SIZE * scale;
        let btn_margin = ICON_BUTTON_LOGICAL_MARGIN * scale;
        let btn_gap = ICON_BUTTON_LOGICAL_GAP * scale;
        // Toolbar band runs from `toolbar_top_phys` to `top_inset`.
        // Center the square buttons vertically inside it.  When the
        // caller hands us 0.0 (snapshot / bench paths with no split
        // header), fall back to the legacy "sit at the top of the
        // inset with `btn_margin` padding" anchoring.
        let toolbar_top = if toolbar_top_phys > 0.0 {
            toolbar_top_phys
        } else {
            self.top_inset
        };
        let toolbar_h = (self.top_inset - toolbar_top).max(btn_size);
        let btn_y = toolbar_top + ((toolbar_h - btn_size) * 0.5).max(0.0);
        // Both icon buttons anchored to the LEFT of the toolbar so
        // the user's hand path stays on one side; sidebar toggle
        // first (it stays visible when sidebar is collapsed and is
        // the most-used affordance), layout picker right of it.
        // Anchored to the window's left edge, NOT to the sidebar's
        // right edge — the title strip + toolbar both extend full
        // width over the sidebar area, so window-left is the
        // consistent anchor whether sidebar is shown or collapsed.
        let btn_w = btn_size;
        let btn_h = btn_size;
        let sidebar_btn_x = btn_margin;
        let layout_btn_x = sidebar_btn_x + btn_size + btn_gap;
        // F3+1 — process-tree toggle sits immediately right of layout
        // picker, anchored to the left toolbar group.  Keeps all three
        // affordances clustered so the user's eye can scan them in one
        // movement.
        let process_btn_x = layout_btn_x + btn_size + btn_gap;
        self.sidebar_button_rect = Rect {
            x: sidebar_btn_x,
            y_top: btn_y,
            w: btn_w,
            h: btn_h,
        };
        self.layout_button_rect = Rect {
            x: layout_btn_x,
            y_top: btn_y,
            w: btn_w,
            h: btn_h,
        };
        self.process_button_rect = Rect {
            x: process_btn_x,
            y_top: btn_y,
            w: btn_w,
            h: btn_h,
        };
        if picker_open {
            let opt_size = PICKER_OPTION_LOGICAL_SIZE * scale;
            let opt_gap = PICKER_OPTION_LOGICAL_GAP * scale;
            let pad = PICKER_PANEL_LOGICAL_PAD * scale;
            let n = PICKER_LAYOUT_DIMS.len();
            let panel_w = pad * 2.0 + opt_size * n as f64 + opt_gap * (n - 1) as f64;
            let panel_h = pad * 2.0 + opt_size;
            // Left-align the picker panel under the layout button so
            // the panel's left edge sits exactly under the button
            // that opened it.  Clamp against the window right edge
            // so the panel never runs off-screen on small windows.
            let panel_x = layout_btn_x.min(self.window_w - panel_w - 4.0).max(4.0);
            let panel_y = btn_y + btn_h + PICKER_PANEL_LOGICAL_GAP_FROM_BUTTON * scale;
            self.picker_panel_rect = Some(Rect {
                x: panel_x,
                y_top: panel_y,
                w: panel_w,
                h: panel_h,
            });

            let mut options = Vec::with_capacity(n);
            let mut dims = Vec::with_capacity(n);
            for (i, &(c, r)) in PICKER_LAYOUT_DIMS.iter().enumerate() {
                let ox = panel_x + pad + i as f64 * (opt_size + opt_gap);
                let oy = panel_y + pad;
                options.push(Rect {
                    x: ox,
                    y_top: oy,
                    w: opt_size,
                    h: opt_size,
                });
                dims.push((c, r));
            }
            self.picker_option_rects = options;
            self.picker_option_dims = dims;
        }

        // Sidebar close-[×] rects.  Geometry uses
        // `sidebar_top_pad_phys` (set by `build`) so it always lines
        // up with the visually painted row — both renderers consume
        // the same value.  Skip when the sidebar is absent
        // (snapshot / 1-pane bench paths).
        if self.sidebar_w > 0.0 && n_sessions > 0 {
            let mut rects = Vec::with_capacity(n_sessions);
            let close_size = SIDEBAR_CLOSE_PHYS_SIZE;
            let close_x = self.sidebar_w
                - SIDEBAR_CLOSE_PHYS_MARGIN_RIGHT
                - close_size;
            for i in 0..n_sessions {
                let row_top = self.top_inset
                    + self.sidebar_top_pad_phys
                    + i as f64 * SIDEBAR_ROW_H_PHYS;
                let close_y = row_top + (SIDEBAR_ROW_H_PHYS - close_size) / 2.0;
                rects.push(Rect {
                    x: close_x,
                    y_top: close_y,
                    w: close_size,
                    h: close_size,
                });
            }
            self.close_session_rects = rects;
        }

        // [+] add-session button — sits in the header band above
        // row 0, spans (most of) the sidebar width.  Always built
        // when sidebar exists; renderer paints it disabled when
        // n_sessions >= 9 and `mouse_down` ignores the click.
        if self.sidebar_w > 0.0 {
            let x = SIDEBAR_ADD_BTN_X_PAD_PHYS;
            let w = (self.sidebar_w - 2.0 * SIDEBAR_ADD_BTN_X_PAD_PHYS).max(1.0);
            let y = self.top_inset + SIDEBAR_HEADER_GAP_PHYS;
            self.add_session_button_rect = Rect {
                x,
                y_top: y,
                w,
                h: SIDEBAR_ADD_BTN_H_PHYS,
            };
        }

        self
    }

    /// True when `(px, py)` falls inside the floating layout button.
    pub fn hit_test_layout_button(&self, px: f64, py: f64) -> bool {
        self.layout_button_rect.contains(px, py)
    }

    /// True when `(px, py)` falls inside the floating sidebar toggle
    /// button.  Always live (the button stays visible even when the
    /// sidebar is collapsed, since it's the only way back).
    pub fn hit_test_sidebar_button(&self, px: f64, py: f64) -> bool {
        self.sidebar_button_rect.contains(px, py)
    }

    /// F3+1 — true when `(px, py)` falls inside the floating process-
    /// tree toggle button.
    pub fn hit_test_process_button(&self, px: f64, py: f64) -> bool {
        self.process_button_rect.contains(px, py)
    }

    /// Returns the picker option index (0..7) the click landed in,
    /// or `None`.  Always `None` when the picker is closed.
    pub fn hit_test_picker_option(&self, px: f64, py: f64) -> Option<usize> {
        self.picker_option_rects
            .iter()
            .position(|r| r.contains(px, py))
    }

    /// True when the picker is open AND `(px, py)` is inside its
    /// panel (used to swallow clicks that hit the panel BG but
    /// missed any option — they close the picker without firing
    /// any cell / sidebar action).
    pub fn hit_test_picker_panel(&self, px: f64, py: f64) -> bool {
        self.picker_panel_rect
            .map(|r| r.contains(px, py))
            .unwrap_or(false)
    }

    /// Returns the sidebar row index whose close-[×] button
    /// `(px, py)` falls inside, or `None`.  Hit-target lives on the
    /// row's right edge; clicks to its left fall through to
    /// `hit_test_sidebar_row` for normal focus switching.
    pub fn hit_test_close_session(&self, px: f64, py: f64) -> Option<usize> {
        self.close_session_rects
            .iter()
            .position(|r| r.contains(px, py))
    }

    /// True when `(px, py)` lands on the sidebar's [+] add-session
    /// button.  The button is always present when the sidebar is
    /// shown; the renderer dims it (and `mouse_down` ignores the
    /// click) when `sessions.len() >= 9`.
    pub fn hit_test_add_session_button(&self, px: f64, py: f64) -> bool {
        self.add_session_button_rect.contains(px, py)
    }

    /// Hit-test a click at physical coords `(px, py)`.  Returns the
    /// session index when the click landed inside one of the grid
    /// cells, else `None` (sidebar hit, or outside the window).  For
    /// sidebar clicks, see [`hit_test_sidebar_row`](Self::hit_test_sidebar_row).
    pub fn hit_test(&self, px: f64, py: f64) -> Option<usize> {
        if px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px >= c.x && px < c.x + c.w && py >= c.y_top && py < c.y_top + c.h {
                return Some(i);
            }
        }
        None
    }

    /// Map a click in the terminal-content area of a cell to its
    /// `(session_idx, col, row)` cell coordinates.  Returns `None`
    /// when the click is in the sidebar, the title strip, the
    /// padding band, or outside the window.  Used by the selection
    /// machinery to anchor / extend a selection on mouse drag.
    /// `cell_w` / `cell_h` are the per-glyph monospace metrics in
    /// physical pixels; the renderer is the source of truth for
    /// these so the caller passes them through.
    pub fn hit_test_cell_pos(
        &self,
        px: f64,
        py: f64,
        cell_w: f64,
        cell_h: f64,
    ) -> Option<(usize, u16, u16)> {
        if px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px < c.x || px >= c.x + c.w || py < c.y_top || py >= c.y_top + c.h {
                continue;
            }
            // Inside the rect — discard hits in the title strip and
            // the surrounding padding so the selection only anchors
            // on actual terminal content.
            let inner_x = c.x + self.padding;
            let inner_y = c.y_top + self.cell_title_h + self.padding;
            if py < inner_y {
                return None;
            }
            let dx = px - inner_x;
            let dy = py - inner_y;
            if dx < 0.0 {
                return None;
            }
            let col = (dx / cell_w).floor().max(0.0) as u16;
            let row = (dy / cell_h).floor().max(0.0) as u16;
            // Clamp to the cell's reported terminal dims so a
            // drag past the right / bottom edge doesn't escape.
            let col = col.min(c.cols.saturating_sub(1));
            let row = row.min(c.rows.saturating_sub(1));
            return Some((i, col, row));
        }
        None
    }

    /// Inverse of `hit_test_cell_pos`: given a cell index + `(col,
    /// row)` inside that cell's terminal grid, return the physical-pixel
    /// rect that one character cell occupies in view-local top-left
    /// coordinates (y-down — same space the renderer paints in).
    /// Returns `None` when `cell_idx` is out of range.  Both
    /// `MarspotApp` and `mcli` use this to publish the focused caret
    /// to the IME (`MarspotAppCtx::set_caret_rect_phys`).
    pub fn caret_view_phys_rect(
        &self,
        cell_idx: usize,
        col: u16,
        row: u16,
        cell_w: f64,
        cell_h: f64,
    ) -> Option<(f64, f64, f64, f64)> {
        let c = self.cells.get(cell_idx)?;
        let inner_x = c.x + self.padding;
        let inner_y = c.y_top + self.cell_title_h + self.padding;
        let x = inner_x + col as f64 * cell_w;
        let y = inner_y + row as f64 * cell_h;
        Some((x, y, cell_w, cell_h))
    }

    /// Hit-test the per-cell title strip — the band at the top of
    /// each cell where `cell_title_h` reserves space for the
    /// session label.  Returns `Some(i)` if the click landed in
    /// cell `i`'s title strip, else `None`.  Used for click-to-edit
    /// title behaviour without disturbing terminal-area clicks.
    pub fn hit_test_cell_title(&self, px: f64, py: f64) -> Option<usize> {
        if self.cell_title_h <= 0.0 || px < self.sidebar_w {
            return None;
        }
        for (i, c) in self.cells.iter().enumerate() {
            if px >= c.x
                && px < c.x + c.w
                && py >= c.y_top
                && py < c.y_top + self.cell_title_h
            {
                return Some(i);
            }
        }
        None
    }

    /// Hit-test the per-cell refresh affordance — a square at the right
    /// edge of cell `i`'s title strip (`cell_title_h` wide, the full strip
    /// height), where the renderer draws the deferred-update glyph (target
    /// #4 step 5b).  Returns `Some(i)` if the click landed there.  The
    /// caller only acts on it when that pane actually has an update staged;
    /// the generous square (vs. the exact glyph advance the renderer uses)
    /// keeps it a comfortable click target.
    pub fn hit_test_cell_refresh(&self, px: f64, py: f64) -> Option<usize> {
        if self.cell_title_h <= 0.0 || px < self.sidebar_w {
            return None;
        }
        let icon_w = self.cell_title_h;
        for (i, c) in self.cells.iter().enumerate() {
            let right = c.x + c.w - self.padding;
            let left = right - icon_w;
            if px >= left && px < right && py >= c.y_top && py < c.y_top + self.cell_title_h {
                return Some(i);
            }
        }
        None
    }

    /// Map a click in the sidebar to the session-list row index.
    /// Returns `None` if the click was outside the sidebar or above /
    /// below the entry list.  `row_height_phys` is the per-entry
    /// height the renderer used (in physical pixels), and `top_pad`
    /// is the gap between the window's top edge and the first entry.
    pub fn hit_test_sidebar_row(
        &self,
        px: f64,
        py: f64,
        top_pad: f64,
        row_height_phys: f64,
        rows: usize,
    ) -> Option<usize> {
        if px < 0.0 || px >= self.sidebar_w || py < top_pad {
            return None;
        }
        let idx = ((py - top_pad) / row_height_phys).floor() as usize;
        if idx >= rows {
            None
        } else {
            Some(idx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_by_three_with_sidebar_uses_full_width() {
        // Cells flush to the outer cell area; 2 px inter-cell gutter
        // only between cells.  When a sidebar is present, an extra
        // 2 px seam strip sits between sidebar and grid for the
        // sidebar-to-grid region hairline.
        let gutter = 1.0;
        let sidebar_seam = 1.0;
        let l = Layout::build(1440.0, 900.0, 200.0, 0.0, 0.0, 3, 3, 8.0, 16.0);
        assert_eq!(l.cells.len(), 9);

        // First column flush against the sidebar seam.
        assert!((l.cells[0].x - (200.0 + sidebar_seam)).abs() < 1e-6);
        // Cells share `inner_w = avail_w - 2 * gutter` equally,
        // where avail_w excludes the sidebar AND the seam.
        let avail_w = 1440.0 - 200.0 - sidebar_seam;
        let cell_w = (avail_w - 2.0 * gutter) / 3.0;
        // Third column at sidebar_w + seam + 2 * (cell_w + gutter).
        assert!(
            (l.cells[2].x
                - (200.0 + sidebar_seam + 2.0 * (cell_w + gutter)))
                .abs()
                < 1e-6
        );
        // Last cell's right edge flush against the window's right.
        assert!(
            ((l.cells[2].x + l.cells[2].w) - 1440.0).abs() < 1e-6
        );

        // Inner content area (where cols/rows are counted) is the
        // cell rect shrunk by 2 * padding.
        let cell_h = (900.0 - 2.0 * gutter) / 3.0;
        let inner_w = cell_w - 2.0 * l.padding;
        let inner_h = cell_h - 2.0 * l.padding;
        assert_eq!(l.cells[0].cols, (inner_w / 8.0).floor() as u16);
        assert_eq!(l.cells[0].rows, (inner_h / 16.0).floor() as u16);
    }

    #[test]
    fn hit_test_cell_refresh_only_at_title_right_edge() {
        // 2×2 with a 20 px title strip, no sidebar.
        let l = Layout::build(1000.0, 600.0, 0.0, 0.0, 20.0, 2, 2, 8.0, 16.0);
        let c = &l.cells[1]; // top-right cell
        let icon_w = l.cell_title_h;
        let right = c.x + c.w - l.padding;
        // A point in the icon square (right edge of the title strip).
        let px = right - icon_w / 2.0;
        let py = c.y_top + l.cell_title_h / 2.0;
        assert_eq!(l.hit_test_cell_refresh(px, py), Some(1));
        // Left side of the same title strip → not the icon.
        assert_eq!(l.hit_test_cell_refresh(c.x + 2.0, py), None);
        // Below the title strip (terminal body) → None even at the right.
        assert_eq!(
            l.hit_test_cell_refresh(px, c.y_top + l.cell_title_h + 5.0),
            None
        );
        // No title strip → never hits.
        let l0 = Layout::build(1000.0, 600.0, 0.0, 0.0, 0.0, 2, 2, 8.0, 16.0);
        assert_eq!(l0.hit_test_cell_refresh(px, py), None);
    }

    #[test]
    fn hit_test_picks_correct_cell_or_sidebar() {
        let l = Layout::build(1000.0, 600.0, 200.0, 0.0, 0.0, 2, 2, 8.0, 16.0);
        // Click in sidebar
        assert_eq!(l.hit_test(50.0, 300.0), None);
        // Click in top-left cell
        assert_eq!(l.hit_test(250.0, 50.0), Some(0));
        // Click in top-right cell
        assert_eq!(l.hit_test(800.0, 50.0), Some(1));
        // Click in bottom-left cell
        assert_eq!(l.hit_test(250.0, 400.0), Some(2));
        // Click in bottom-right cell
        assert_eq!(l.hit_test(800.0, 400.0), Some(3));
    }

    #[test]
    fn zero_sidebar_means_grid_uses_whole_window() {
        // 3×1 has > 1 cell so a gutter applies *between* cells —
        // first cell is flush at x=0, last cell flush at the right.
        let gutter = 1.0;
        let l = Layout::build(900.0, 600.0, 0.0, 0.0, 0.0, 3, 1, 8.0, 16.0);
        assert!(l.cells[0].x.abs() < 1e-6);
        let cell_w = (900.0 - 2.0 * gutter) / 3.0;
        assert!(
            (l.cells[2].x - 2.0 * (cell_w + gutter)).abs() < 1e-6
        );
        // Last cell's right edge flush against window_w.
        assert!(
            ((l.cells[2].x + l.cells[2].w) - 900.0).abs() < 1e-6
        );
    }
}
