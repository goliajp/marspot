// CARVE-OUT: pre-existing file: the window layout of sidebar and session cells; shrink on every touch
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

mod hit;
mod rect;
mod strip;

pub use rect::{Alignment, CellRect, Rect};
pub use strip::{cell_grid, CellGrid, STRIP_MIN_TOTAL_ROWS};

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
    /// cc — toolbar `Cc` button (Claude profile usage modal).
    pub cc_button_rect: Rect,
    /// Toolbar button #6 — the settings panel.
    pub settings_button_rect: Rect,
    // F3+3.0 — picker popup removed.  The toolbar layout button
    // now opens a `LayoutModal` (see `src/ui/components/`) where
    // the user sets arbitrary cols × rows.  The modal is rendered
    // by the chrome layer, not layout.rs — Layout no longer carries
    // picker-overlay rects.
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
    /// Different for every layout made: by `build`, and again by
    /// `with_chrome`.  The fields are only ever set there, so two
    /// layouts with one generation are the same layout, and whoever
    /// keeps something worked out from one can tell it is still good
    /// by comparing one number.
    pub generation: u64,
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Square icon-button side length (logical pt). Both chrome buttons
/// (sidebar toggle, layout picker) are now true squares — Lucide-
/// style monochrome line icons sit inside a 22×22 hit-target.
pub const ICON_BUTTON_LOGICAL_SIZE: f64 = 22.0;
pub const ICON_BUTTON_LOGICAL_GAP: f64 = 6.0;

/// Vertical center of the macOS traffic-light row, in logical pt from
/// the window's top edge.  AppKit draws the window buttons itself at a
/// fixed offset that ignores our chrome height, so this is measured,
/// not derived: screen-captured the live window at its frame origin and
/// found the 14pt discs spanning y 9..23 — center 16.  (The capture
/// self-calibrates: the toolbar buttons in the same frame landed on
/// exactly the row this constant asked for, so the capture's origin and
/// the layout's origin are known to agree.)  The toolbar buttons center
/// on this same row, which is what makes the header read as one line
/// instead of a title strip stacked over a toolbar.
pub const TRAFFIC_LIGHT_CENTER_Y_LOGICAL: f64 = 16.0;
/// Left edge of the toolbar's first icon button, logical pt from the
/// window's left edge.  Same capture: the light group runs x 9..69, so
/// this leaves a 15pt gap — wide enough that the OS's cluster and ours
/// read as two groups instead of one crowded run of controls.
const TOOLBAR_LEFT_LOGICAL: f64 = 84.0;
/// Gap between the OS's window-button cluster and our first icon —
/// wide enough that the two read as two groups rather than one
/// crowded run of controls.  (The old constant above bakes the same
/// 15 pt in against a measured cluster edge of 69.)
const TOOLBAR_LIGHTS_GAP_LOGICAL: f64 = 15.0;
/// Left margin when there is no cluster to clear.  The same breathing
/// room the cluster itself gets from the window edge.
const TOOLBAR_EDGE_MARGIN_LOGICAL: f64 = 9.0;

static DENSITY_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1.0f64.to_bits());

/// Physical pixels per window unit: the display's backing scale.
/// Every size this module names is in window units and is multiplied
/// by it; a row drawn 22 physical pixels high is half its size on a 2×
/// display.  What comes out of `Layout` is physical pixels.
pub fn density() -> f64 {
    f64::from_bits(DENSITY_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

pub fn set_density(scale: f64) {
    DENSITY_BITS.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// Height of a sidebar row in physical pixels.
pub fn sidebar_row_h() -> f64 {
    SIDEBAR_ROW_H * density()
}

/// Sidebar geometry, in window units.  Renderer reads
/// `Layout::sidebar_top_pad_phys` (computed at build time so the
/// header band reserved for the [+] add-session button is accounted
/// for).
pub const SIDEBAR_ROW_H: f64 = 22.0;
const SIDEBAR_HEADER_GAP: f64 = 8.0;
const SIDEBAR_ADD_BTN_H: f64 = 22.0;
const SIDEBAR_BODY_GAP: f64 = 8.0;
const SIDEBAR_ADD_BTN_X_PAD: f64 = 8.0;
const SIDEBAR_CLOSE_SIZE: f64 = 14.0;
const SIDEBAR_CLOSE_MARGIN_RIGHT: f64 = 8.0;
/// Width of the line between two panes and between the sidebar and
/// the grid, and a pane's inner padding, in window units.
const SEAM: f64 = 1.0;
const PANE_PADDING: f64 = 8.0;

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
        // With a sidebar, one seam's width is kept between it and the
        // grid for the line that divides them.
        let sidebar_seam = if sidebar_w > 0.0 { SEAM * density() } else { 0.0 };
        let avail_w = (window_w - sidebar_w - sidebar_seam).max(0.0);
        let avail_h = (window_h - top_inset).max(0.0);
        // The strip between two panes, so a grid reads as a grid.
        // None when there is one pane and nothing to divide.
        let gutter = if grid_cols > 1 || grid_rows > 1 { SEAM * density() } else { 0.0 };
        // Room between a pane's edge and its first and last column and
        // row, so the text does not start in the pane's corner pixel.
        let padding = PANE_PADDING * density();

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
        let cell_inner_h = (cell_phys_h - cell_title_h - 2.0 * padding).max(1.0);

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
                    strip_rows: 0,
                    term_bottom: y_top + cell_phys_h,
                });
            }
        }
        // Sidebar-top pad: header gap + add-session button + body
        // gap when the sidebar is shown.  Zero otherwise (snapshot /
        // bench paths don't reserve a header band).  Computed here
        // (not in `with_chrome`) because the renderer's row math
        // reads it whether or not chrome was layered on.
        let sidebar_top_pad_phys = if sidebar_w > 0.0 {
            (SIDEBAR_HEADER_GAP + SIDEBAR_ADD_BTN_H + SIDEBAR_BODY_GAP) * density()
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
            cc_button_rect: Rect::ZERO,
            settings_button_rect: Rect::ZERO,
            close_session_rects: Vec::new(),
            add_session_button_rect: Rect::ZERO,
            sidebar_top_pad_phys,
            generation: next_generation(),
        }
    }

    /// Layer the floating-chrome rects (the [layout] button, the
    /// picker overlay when `picker_open`, and the per-row close [×]
    /// rects) onto an already-built `Layout`.  Done as a post-build
    /// step so snapshot / bench / test paths that don't render
    /// chrome can call the original `build` unchanged.
    /// `scale` is the device pixel ratio (caller already has it
    /// from `ctx.scale()`); we use it for the button + picker
    /// (logical-pt sizing).  `n_sessions` populates the close-[×]
    /// rects (one per row).
    /// `lights_right_phys` is where the OS's own window buttons end,
    /// as measured by the shell: `Some(0.0)` when they are not on
    /// screen, and **`None` when nobody has measured yet**.
    ///
    /// Those two are not the same thing and collapsing them put the
    /// toolbar straight on top of the buttons: a freshly built window
    /// has no measurement, `0.0` was read as "no cluster", and the
    /// icons drew over the OS's own (2026-08-11).  Unknown falls back
    /// to the historical constant, which is right for the overwhelming
    /// case (a normal window) and wrong only until the first frame
    /// arrives.
    pub fn with_chrome(mut self, scale: f64, n_sessions: usize, lights_right_phys: Option<f64>) -> Self {
        let btn_size = ICON_BUTTON_LOGICAL_SIZE * scale;
        let btn_gap = ICON_BUTTON_LOGICAL_GAP * scale;
        // Buttons center on the traffic-light row, NOT inside the
        // header band — the lights are AppKit's and sit at a fixed
        // offset from the window top, so that row is the anchor the
        // whole header lines up against (`HEADER_PT` is sized to
        // agree with it, but this math doesn't depend on that).
        let btn_y = TRAFFIC_LIGHT_CENTER_Y_LOGICAL * scale - btn_size * 0.5;
        // Icon buttons anchored to the LEFT of the header, starting
        // right of the traffic lights so the user's hand path stays on
        // one side; sidebar toggle first (it stays visible when the
        // sidebar is collapsed and is the most-used affordance),
        // layout picker right of it.
        // Anchored to the window's left edge, NOT to the sidebar's
        // right edge — the header extends full width over the sidebar
        // area, so window-left is the consistent anchor whether the
        // sidebar is shown or collapsed.
        let btn_w = btn_size;
        let btn_h = btn_size;
        // The toolbar starts after the OS's cluster, wherever that
        // ends — and when the cluster is gone (full screen), it starts
        // at the window's own margin instead of leaving a hole where
        // the buttons used to be (2026-08-11 report).
        //
        // `TOOLBAR_LEFT_LOGICAL` survives only as the fallback for a
        // shell too old to send the measurement.
        let sidebar_btn_x = match lights_right_phys {
            Some(right) if right > 0.0 => right + TOOLBAR_LIGHTS_GAP_LOGICAL * scale,
            Some(_) => TOOLBAR_EDGE_MARGIN_LOGICAL * scale,
            None => TOOLBAR_LEFT_LOGICAL * scale,
        };
        let layout_btn_x = sidebar_btn_x + btn_size + btn_gap;
        // F3+1 — process-tree toggle sits immediately right of layout
        // picker, anchored to the left toolbar group.  Keeps all three
        // affordances clustered so the user's eye can scan them in one
        // movement.
        let process_btn_x = layout_btn_x + btn_size + btn_gap;
        let cc_btn_x = process_btn_x + btn_size + btn_gap;
        let settings_btn_x = cc_btn_x + btn_size + btn_gap;
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
        self.cc_button_rect = Rect {
            x: cc_btn_x,
            y_top: btn_y,
            w: btn_w,
            h: btn_h,
        };
        self.settings_button_rect = Rect {
            x: settings_btn_x,
            y_top: btn_y,
            w: btn_w,
            h: btn_h,
        };
        // Sidebar close-[×] rects.  Geometry uses
        // `sidebar_top_pad_phys` (set by `build`) so it always lines
        // up with the visually painted row — both renderers consume
        // the same value.  Skip when the sidebar is absent
        // (snapshot / 1-pane bench paths).
        if self.sidebar_w > 0.0 && n_sessions > 0 {
            let mut rects = Vec::with_capacity(n_sessions);
            let close_size = SIDEBAR_CLOSE_SIZE * density();
            let close_x = self.sidebar_w - SIDEBAR_CLOSE_MARGIN_RIGHT * density() - close_size;
            for i in 0..n_sessions {
                let row_top =
                    self.top_inset + self.sidebar_top_pad_phys + i as f64 * sidebar_row_h();
                let close_y = row_top + (sidebar_row_h() - close_size) / 2.0;
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
            let x = SIDEBAR_ADD_BTN_X_PAD * density();
            let w = (self.sidebar_w - 2.0 * x).max(1.0);
            let y = self.top_inset + SIDEBAR_HEADER_GAP * density();
            self.add_session_button_rect = Rect {
                x,
                y_top: y,
                w,
                h: SIDEBAR_ADD_BTN_H * density(),
            };
        }

        self.generation = next_generation();
        self
    }

    /// Every toolbar button, in left-to-right order.
    ///
    /// The painter iterates **this**, zipped against a same-length
    /// array of icons, so a button that exists in the layout cannot be
    /// left undrawn — which is exactly what happened when the settings
    /// button was added (2026-08-08: the rect was laid out, the
    /// hit-test worked, and nothing was painted, so the toolbar had an
    /// invisible sixth button for an hour).  A mismatched icon list
    /// now fails to compile.
    pub fn toolbar_buttons(&self) -> [Rect; 5] {
        [
            self.sidebar_button_rect,
            self.layout_button_rect,
            self.process_button_rect,
            self.cc_button_rect,
            self.settings_button_rect,
        ]
    }
}

#[cfg(test)]
mod tests {

    /// The toolbar's buttons must be laid out where a user can hit
    /// them, in order, without overlapping — and `toolbar_buttons()`
    /// must list every one of them.
    ///
    /// The list is what the painter iterates.  A button missing from
    /// it is a button that exists (it has a rect, its hit-test works)
    /// and is never drawn — which is how the settings button shipped
    /// invisible on 2026-08-08.
    #[test]
    fn every_toolbar_button_is_laid_out_and_listed() {
        // `with_chrome` is what lays the toolbar out; `build` alone
        // makes the chrome-less layout the bench and snapshot paths use.
        let l = Layout::build(1400.0, 900.0, 0.0, 40.0, 20.0, 2, 2, 8.0, 16.0)
            .with_chrome(2.0, 2, Some(138.0));
        let btns = l.toolbar_buttons();
        assert_eq!(btns.len(), 5, "add the icon too, or it will not be painted");
        for (i, b) in btns.iter().enumerate() {
            assert!(b.w > 0.0 && b.h > 0.0, "button {i} has no area");
            assert!(b.x >= 0.0, "button {i} starts off-screen");
            assert!(
                b.x + b.w <= l.window_w,
                "button {i} runs past the window edge"
            );
            if i > 0 {
                let prev = btns[i - 1];
                assert!(b.x >= prev.x + prev.w, "button {i} overlaps its neighbour");
            }
        }
        assert_eq!(btns[3], l.cc_button_rect);
        assert_eq!(btns[4], l.settings_button_rect);
    }

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
        assert!((l.cells[2].x - (200.0 + sidebar_seam + 2.0 * (cell_w + gutter))).abs() < 1e-6);
        // Last cell's right edge flush against the window's right.
        assert!(((l.cells[2].x + l.cells[2].w) - 1440.0).abs() < 1e-6);

        // Inner content area (where cols/rows are counted) is the
        // cell rect shrunk by 2 * padding.
        let cell_h = (900.0 - 2.0 * gutter) / 3.0;
        let inner_w = cell_w - 2.0 * l.padding;
        let inner_h = cell_h - 2.0 * l.padding;
        assert_eq!(l.cells[0].cols, (inner_w / 8.0).floor() as u16);
        assert_eq!(l.cells[0].rows, (inner_h / 16.0).floor() as u16);
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

    /// Full screen takes the OS's window buttons away; the toolbar
    /// has to move into the space they left rather than sit to the
    /// right of a hole (2026-08-11 report).
    #[test]
    fn the_toolbar_follows_the_window_buttons() {
        let windowed = Layout::build(1600.0, 900.0, 0.0, 40.0, 0.0, 1, 1, 8.0, 16.0).with_chrome(
            2.0,
            1,
            Some(138.0),
        );
        let full = Layout::build(1600.0, 900.0, 0.0, 40.0, 0.0, 1, 1, 8.0, 16.0).with_chrome(
            2.0,
            1,
            Some(0.0),
        );
        // Not measured yet is its own answer: keep clear of where the
        // buttons normally are, rather than assuming they are gone.
        let unknown =
            Layout::build(1600.0, 900.0, 0.0, 40.0, 0.0, 1, 1, 8.0, 16.0).with_chrome(2.0, 1, None);
        assert!(
            unknown.toolbar_buttons()[0].x >= 84.0 * 2.0 - 1e-6,
            "an unmeasured window must not put its toolbar on the OS's buttons",
        );

        let w0 = windowed.toolbar_buttons()[0];
        let f0 = full.toolbar_buttons()[0];
        assert!(
            w0.x > 138.0,
            "windowed: the toolbar starts after the cluster, not on it",
        );
        assert!(
            f0.x < w0.x,
            "full screen: the toolbar moves left into the freed space \
             ({:.0} should be left of {:.0})",
            f0.x,
            w0.x,
        );
        assert!(f0.x > 0.0, "…but keeps a margin off the window edge");
        // Everything shifts together — the group keeps its shape.
        let wb = windowed.toolbar_buttons();
        let fb = full.toolbar_buttons();
        let shift = wb[0].x - fb[0].x;
        for (a, b) in wb.iter().zip(fb.iter()).filter(|(a, _)| a.w > 0.0) {
            assert!((a.x - b.x - shift).abs() < 1e-6, "the toolbar came apart");
            assert!((a.y_top - b.y_top).abs() < 1e-6, "and it stays on its row");
        }
    }

    #[test]
    fn zero_sidebar_means_grid_uses_whole_window() {
        // 3×1 has > 1 cell so a gutter applies *between* cells —
        // first cell is flush at x=0, last cell flush at the right.
        let gutter = 1.0;
        let l = Layout::build(900.0, 600.0, 0.0, 0.0, 0.0, 3, 1, 8.0, 16.0);
        assert!(l.cells[0].x.abs() < 1e-6);
        let cell_w = (900.0 - 2.0 * gutter) / 3.0;
        assert!((l.cells[2].x - 2.0 * (cell_w + gutter)).abs() < 1e-6);
        // Last cell's right edge flush against window_w.
        assert!(((l.cells[2].x + l.cells[2].w) - 900.0).abs() < 1e-6);
    }
}
