//! The parts of a `SessionView` that are types of their own.

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
