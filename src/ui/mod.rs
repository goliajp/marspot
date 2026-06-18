//! Multi-pane UI primitives shared by the two front-ends that drive
//! the full marspot experience: the standalone `marspot` binary
//! (AppKit window, src/main.rs) and `marspot-core` (renders into the
//! shell's IOSurface, input arrives over the control socket).  Both
//! own the same conceptual state — panes, focus, layout mode,
//! sidebar, selection — so the shapes and the tricky pure logic
//! (selection projection / serialisation, scroll math) live here;
//! each front-end keeps only its event plumbing.

/// F3+1.5 — reusable visual components.  Start of a deliberate
/// "marspot UI kit": small composable pieces (traffic lights, tab
/// strip, scroll view, modal frame) that any feature wanting a
/// macOS-ish look should reach for instead of hand-rolling the
/// fifth slightly-different SDF rect.  See each submodule for its
/// contract.
pub mod components;

use crate::pane::Pane;
use crate::render::SelectionView;

/// Switchable session-grid layouts (mouse-driven via the [layout]
/// button in the main area).  Cell counts ∈ {1, 2, 4, 6, 9}; 2 and 6
/// have horizontal / vertical orientation variants.  Sessions live
/// independently — the layout decides how many cells get rendered in
/// the main area, not how many sessions exist.  When N sessions <
/// cells, extra cells render as empty placeholders; when N > cells,
/// extra sessions stay in the sidebar but don't get a main-area cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Single,
    SplitH,
    SplitV,
    Quad,
    SixH,
    SixV,
    Nine,
}

impl LayoutMode {
    /// `(grid_cols, grid_rows)` — the shape passed straight to
    /// `Layout::build`.  Names follow the orientation of the *split*:
    /// `SplitH` is a horizontal split = 2 cells side by side = (2,1).
    pub fn dims(self) -> (usize, usize) {
        match self {
            Self::Single => (1, 1),
            Self::SplitH => (2, 1),
            Self::SplitV => (1, 2),
            Self::Quad => (2, 2),
            Self::SixH => (3, 2),
            Self::SixV => (2, 3),
            Self::Nine => (3, 3),
        }
    }
    pub fn cells(self) -> usize {
        let (c, r) = self.dims();
        c * r
    }
}

/// Picker option index → LayoutMode.  Order must match
/// `layout::PICKER_LAYOUT_DIMS` (private to layout.rs but the
/// dims line up): Single, SplitH, SplitV, Quad, SixH, SixV, Nine.
pub const PICKER_LAYOUTS: [LayoutMode; 7] = [
    LayoutMode::Single,
    LayoutMode::SplitH,
    LayoutMode::SplitV,
    LayoutMode::Quad,
    LayoutMode::SixH,
    LayoutMode::SixV,
    LayoutMode::Nine,
];

pub const SIDEBAR_W_LOGICAL: f64 = 200.0;

/// Per-cell title strip height in **logical points** — the band at
/// the top of every 9-grid cell that shows the session label and a
/// SEAM hairline below.  Layout reserves it inside the cell rect;
/// the renderer paints title text + bottom seam.  Tuned to fit one
/// 12-pt monospace line plus 6 pt of breathing room.
pub const CELL_TITLE_PT: f64 = 22.0;

/// Sidebar label cap — at the default 200-pt sidebar with Monaco
/// 12-pt metrics, ~22 ASCII glyphs fit between the dot+gap and the
/// right edge.  Anything longer is truncated with `...` (three
/// ASCII dots — same monospace cell width as the rest of the
/// label, plays nicer with the user's preference than `…`).
pub const MAX_SIDEBAR_LABEL_CHARS: usize = 22;

/// Hard cap on how many sessions marspot permits at once.  The
/// sidebar [+] button is disabled past this count; the layout
/// picker only offers shapes whose cells ≤ cap (== 9).
pub const SESSION_COUNT_HARD_CAP: usize = 9;

/// Truncate a sidebar label to at most `max_chars` total characters,
/// replacing the dropped tail with three ASCII dots.  Counts
/// Unicode scalars, not bytes, so multi-byte characters survive
/// uniformly.  Reserves three trailing slots for `...`.
pub fn truncate_for_sidebar(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars - 3).collect();
    format!("{head}...")
}

/// Live text selection inside one session's grid.  Column is the
/// grid column index; `abs` is "rows up from the current live grid's
/// bottom" — 0 = live bottom row, `rows-1` = live top row, then
/// `rows..=rows+scrollback_len-1` walks scrollback from newest to
/// oldest.  This anchors the selection to *content* (modulo PTY churn,
/// which shifts content into scrollback as new lines come in) instead
/// of to the *viewport*, so scrolling preserves the selection and the
/// user can extend it across the viewport edge into scrollback.
/// `anchor` is where the drag started, `focus` is the current cursor
/// position; serialise/render normalise so the pair always reads
/// top-left → bottom-right.  `mode` is sticky for the duration of one
/// drag (locked at mouse_down based on the Option modifier).
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub session_idx: usize,
    pub anchor: (u16, u32),
    pub focus: (u16, u32),
    pub mode: SelectionMode,
}

/// Linewise: cross-row drags take the first row from `anchor.col` to
/// the end, middle rows entirely, the last row from start to
/// `focus.col` — the iTerm2 / Terminal.app default, ideal for prose.
/// Blockwise: each row is sliced to `[min(anchor.col, focus.col),
/// max(...)]`, so the user can carve out a rectangle inside multi-
/// column output (ls, top, htop) without dragging the column-aligned
/// padding along with it.  Triggered by Option+drag at mouse_down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    Linewise,
    Blockwise,
}

/// Scroll prefs from env, read once:
///   MARSPOT_SCROLL_INVERT=1   flip direction
///   MARSPOT_SCROLL_FACTOR=<f> multiplier; default 1.0
pub fn scroll_config() -> (bool, f64) {
    use std::sync::OnceLock;
    static CFG: OnceLock<(bool, f64)> = OnceLock::new();
    *CFG.get_or_init(|| {
        let invert = std::env::var("MARSPOT_SCROLL_INVERT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let factor = std::env::var("MARSPOT_SCROLL_FACTOR")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|f| *f > 0.0 && *f < 100.0)
            .unwrap_or(1.0);
        (invert, factor)
    })
}

/// Wheel/trackpad delta → signed line count for `apply_scroll_lines`.
/// Matches iTerm2 / native macOS direction with the OS natural-scroll
/// preference on; `scroll_config` env knobs adjust.  Returns 0 when
/// the gesture is below half a line (caller skips the redraw).
pub fn scroll_lines(dy_phys: f64, precise: bool, cell_h: f64) -> i32 {
    let (invert, factor) = scroll_config();
    let sign: f64 = if invert { -1.0 } else { 1.0 };
    let lines_f = if precise {
        sign * factor * dy_phys / cell_h
    } else {
        sign * factor * dy_phys * 3.0
    };
    if lines_f.abs() < 0.5 {
        0
    } else {
        lines_f as i32
    }
}

/// Project a content-anchored `Selection` into viewport coordinates
/// for the renderer.  Coords are abs (rows up from this pane's
/// current live bottom); project to viewport rows using this pane's
/// view_offset, clip to the visible band, and only return Some when
/// at least one row lands on screen.  This is what makes scrolling
/// preserve the selection visual: as view_offset changes the painted
/// rows shift in lockstep with the content under them.
pub fn selection_view_for_pane(
    pane: &Pane,
    sel: &Selection,
    pane_idx: usize,
) -> Option<SelectionView> {
    if sel.session_idx != pane_idx {
        return None;
    }
    let pane_vo = pane.view_offset() as i64;
    // `grid()` (not `terminal().grid()`) so this works for L3 panes too:
    // their mirror is the window L3 published at `view_offset()`, so the
    // abs→viewport mapping below lines up with what's on screen.
    let g_rows = pane.session().grid().rows() as i64;
    let last = g_rows - 1;
    let abs_to_vp = |abs: u32| -> i64 {
        // vp = (rows-1) + vo - abs
        last + pane_vo - abs as i64
    };
    let a_vp = abs_to_vp(sel.anchor.1);
    let f_vp = abs_to_vp(sel.focus.1);
    // Both ends above viewport (vp < 0) or both below (vp > last)
    // → nothing on screen.
    if (a_vp < 0 && f_vp < 0) || (a_vp > last && f_vp > last) {
        return None;
    }
    // Clip each end to the visible band.  For linewise selections, a
    // vp clamped past the top also forces col to the left edge (and
    // past bottom to the right edge), so the painted band extends
    // visually to "the rest of the visible area".  Blockwise must
    // keep the col unchanged — the rectangle's width is defined by
    // the anchor/focus cols regardless of which rows are currently
    // on-screen.
    let blockwise = sel.mode == SelectionMode::Blockwise;
    let max_col_clamp = pane.session().grid().cols().saturating_sub(1);
    let clip = |col: u16, vp: i64| -> (u16, u16) {
        if vp < 0 {
            (if blockwise { col } else { 0 }, 0)
        } else if vp > last {
            (
                if blockwise { col } else { max_col_clamp },
                last as u16,
            )
        } else {
            (col, vp as u16)
        }
    };
    Some(SelectionView {
        anchor: clip(sel.anchor.0, a_vp),
        focus: clip(sel.focus.0, f_vp),
        blockwise,
    })
}

/// Serialise the selected text out of the pane's grid.  Returns
/// `None` when the selection resolves to nothing (empty grid, or
/// all-blank rows).  Trailing whitespace on each row is dropped so a
/// selection that overshoots the line's text doesn't carry a run of
/// spaces; multi-row selections join with `\n`.
/// Serialise an in-process pane's selection to clipboard text.  Delegates
/// to the pure `grid_selection_text` (shared with the L3 session process,
/// which answers L2's `GetSelectionText` over the wire with the same
/// logic).  L3-backed panes have no in-process scrollback to read here —
/// the container requests their text from the session process instead, so
/// this must not be called for them.
pub fn selection_text(pane: &Pane, sel: &Selection) -> Option<String> {
    debug_assert!(
        !pane.is_l3(),
        "selection_text on an L3 pane — request from the session process instead"
    );
    crate::render::grid_selection_text(
        pane.session().grid(),
        sel.anchor,
        sel.focus,
        sel.mode == SelectionMode::Blockwise,
    )
}
