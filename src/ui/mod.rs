//! Multi-pane UI primitives shared by the two front-ends that drive
//! the full marspot experience: the standalone `marspot` binary
//! (AppKit window, src/main.rs) and `marspot-core` (renders into the
//! shell's IOSurface, input arrives over the control socket).  Both
//! own the same conceptual state — panes, focus, layout mode,
//! sidebar, selection — so the shapes and the tricky pure logic
//! (selection projection / serialisation, scroll math) live here;
//! each front-end keeps only its event plumbing.

/// F3+1.8 — marspot UI kit, organised in three tiers:
///
///   `core/`    — foundation primitives every UI reaches for first
///                (the `View` overlay surface, painter, backdrop)
///   `system/`  — platform-specific chrome
///                (`system::macos::traffic_lights`, `::title_bar`)
///   `components/` — composite widgets built on top of core + system
///                (tab strip, scroll view, modal frame, search
///                overlay, …)
///
/// Scene-private rendering (e.g. the Process Monitor's body row layout)
/// lives next to its scene code in `marspot-core`; the UI kit is for
/// the pieces a second scene would also want.
pub mod core;
pub mod system;
pub mod components;
pub mod theme;
pub mod view;

use crate::pane::Pane;
use crate::render::SelectionView;

// F3+3.0 — `LayoutMode` + `PICKER_LAYOUTS` removed.  The grid shape
// is now a free `(cols, rows)` carried in CoreState; the user picks
// it via the `LayoutModal` (toolbar layout button → modal).  Cell
// total = cols × rows; when N sessions < cells extra cells render
// as empty placeholders; when N > cells extras stay alive in the
// sidebar without a main-area cell (unchanged from earlier behaviour).

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

/// Hard cap on how many sessions marspot permits at once.  Sized
/// to match the LayoutModal's GRID_MAX² (6×6 = 36) — the modal
/// won't offer a shape past this, and the sidebar [+] button is
/// disabled past it.  Pre-F3+3.0 this was 9 (locked to the fixed
/// Nine grid); freed once arbitrary cols × rows landed.
pub const SESSION_COUNT_HARD_CAP: usize = 36;

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

/// The display's backing scale, as everything marspot draws must use it.
///
/// **The rule.** Every pixel number in this program was tuned by eye
/// on a display whose `backingScaleFactor` is 1 — so every one of them
/// is a *logical point*, and the physical size it should occupy is
/// `that number × this`.  Terminal cell, panel text, menu rows, modal
/// padding: all of it, one multiplier.
///
/// At 1 that is the identity, which is why adopting the rule changed
/// nothing on the display it was tuned on (verified: the panel
/// snapshots came out byte-identical).  At 2 — any retina Mac —
/// everything doubles **together**, which is the point: the previous
/// arrangement doubled the boxes and left their contents alone.
///
/// Measured 2026-08-11, one menu at both scales before the rule:
///
/// | | scale 1 | scale 2 |
/// |---|---|---|
/// | menu box | 180 × 184 px | 356 × 368 px |
/// | row pitch | 26 px | 52 px |
/// | **label ink** | **12 px** | **12 px** |
///
/// One value for the process, not one per window: the renderer keeps a
/// single `FontCache` and a single terminal cell size, so two windows
/// on displays of different densities are already outside what this
/// architecture expresses.  Set it before the renderer is built.
static CHROME_SCALE_BITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1.0f64.to_bits());

/// Physical pixels per logical point.
pub fn chrome_scale() -> f64 {
    f64::from_bits(CHROME_SCALE_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Adopt the display's backing scale.  Ignores nonsense (0, negative,
/// NaN) rather than letting it reach the layout — a bad scale there
/// collapses every rect in the app to nothing.
pub fn set_chrome_scale(scale: f64) {
    if scale.is_finite() && scale >= 0.5 && scale <= 4.0 {
        CHROME_SCALE_BITS.store(scale.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
}

/// Scroll prefs: direction from the environment, speed from the
/// settings file.
///
///   MARSPOT_SCROLL_INVERT=1   flip direction (read once — it names
///                             the machine, and machines do not change
///                             which way their mouse works mid-run)
///   MARSPOT_SCROLL_FACTOR=<f> multiplier; overrides the setting
///
/// The factor is read **per gesture** rather than cached, which is
/// what lets the settings panel change it with the wheel already under
/// the user's finger.  A wheel event is not a hot path; a lock here
/// costs nothing measurable.
pub fn scroll_config() -> (bool, f64) {
    use std::sync::OnceLock;
    static INVERT: OnceLock<bool> = OnceLock::new();
    static ENV_FACTOR: OnceLock<Option<f64>> = OnceLock::new();
    let invert = *INVERT.get_or_init(|| {
        std::env::var("MARSPOT_SCROLL_INVERT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    });
    let env_factor = *ENV_FACTOR.get_or_init(|| {
        std::env::var("MARSPOT_SCROLL_FACTOR")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|f| *f > 0.0 && *f < 100.0)
    });
    let factor = env_factor.unwrap_or_else(|| crate::settings::get().scroll_factor as f64);
    (invert, factor)
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
