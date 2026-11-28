//! The parts of a `SessionView` that are types of their own, and a
//! sidebar row.

use super::SessionView;
use crate::grid::Grid;
use crate::session::SessionState;

/// A pane's strip: a grid of its own, in the pane's cells.
#[derive(Copy, Clone)]
pub struct StripView<'a> {
    pub grid: &'a Grid,
    /// Changes whenever what `grid` holds does.
    pub seq: u64,
    /// The row of the pane its first row is drawn at, counted from the
    /// terminal's first.  Under the terminal's last, or over rows of it
    /// that are blank.
    pub top_row: u16,
}

impl<'a> SessionView<'a> {
    /// `grid` at its live tail with nothing else to show: no cursor, no
    /// title, no marks.
    pub fn plain(grid: &'a Grid) -> Self {
        SessionView {
            grid,
            view_offset: 0,
            cursor_visible: false,
            focused: false,
            title: "",
            selection: None,
            ime_preedit: "",
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "",
            agent_tui: false,
            cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: HistoryView::default(),
            session: 0,
            strip: None, notice: None,
            seq: 0,
        }
    }
}

/// C4 — viewport-row-keyed highlight span.  Renderer paints
/// `HIGHLIGHT_BG` under `col_start..=col_end_inclusive` on
/// viewport row `view_row`.  L2 main loop translates a focused
/// search hit's `WirePhysicalSpan` into one or more of these by
/// mapping the hit's row indices through the current `view_offset`
/// (scrollback hit ⇒ row = scrollback_idx - (sb_len - view_offset),
/// live hit ⇒ row = grid-local row offset).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub view_row: u16,
    pub col_start: u16,
    pub col_end_inclusive: u16,
}

/// C4 — per-pane active-highlight slot.  `query_id` lets the
/// renderer drop stale highlights when a fresh `SearchScrollback`
/// supersedes the one whose hit is currently highlighted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveHighlight {
    pub query_id: u32,
    pub spans: Vec<HighlightSpan>,
}

/// F1+ — immutable per-frame snapshot of the search overlay, fed to
/// the renderer in `SessionView`.  Owned (not `'a`-borrowed) so the
/// renderer doesn't need to keep `pane.search` borrowed across
/// `build_instances`; the clone cost is paid only when search is
/// open (a String per query + per visible snippet, < 1 µs).  Subset
/// of §6.6.5's `SearchOverlay` fields — minimum needed to paint the
/// bar + a minimal result list.
#[derive(Clone, Debug)]
pub struct SearchOverlayView {
    pub query: String,
    /// Char-column of the edit cursor inside `query`.
    pub query_cursor: u16,
    pub case_sensitive: bool,
    /// `(focused_1based, total)` — drawn as `N/M`.  `None` skips the
    /// counter (e.g. before the first SearchResults arrives).
    pub counter: Option<(u32, u32)>,
    /// Up to `VIEWPORT` hit snippets (newest-first).  Each row's
    /// `is_focused` flag drives the focused-row BG highlight.
    pub hits: Vec<SearchHitView>,
}

/// Per-row view of one hit in the result list.
#[derive(Clone, Debug)]
pub struct SearchHitView {
    pub snippet: String,
    pub is_focused: bool,
}

/// A pane's place in its scrollback: `offset` rows up from live, out
/// of `len` rows of history.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryView {
    pub offset: u16,
    pub len: u16,
}

/// One row of the sidebar — what the user sees on the left.  Length
/// of the slice passed to `MetalRenderer::render_layout` should match
/// `views.len()`; entry `i` describes session `i`.
pub struct SidebarEntry<'a> {
    /// Short label drawn after the state dot, e.g. `"1"`, `"build"`.
    pub label: &'a str,
    /// Drives the colour of the state dot.
    pub state: SessionState,
    /// The pane's four-digit code, drawn as `#4145` before the label.
    pub code: Option<u16>,
}
