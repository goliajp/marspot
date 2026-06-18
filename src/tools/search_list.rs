//! C3 — search result list overlay.  Pairs with `SearchBar` (C2):
//! the bar collects the query, this struct collects + presents the
//! hits.  Spec: `docs/scrollback-search.md` §6.5 + §6.6.5 + §6.7 +
//! §6.8 + §6.9 + §12.C3.
//!
//! Same C2 spirit — data + behaviour FSM, no rendering yet.  C5
//! wires the actual frame consumption (L2 receives `SearchResults`
//! → calls `apply_results`) and the focus jump (Enter / click →
//! pane.view_offset updates).

use std::collections::VecDeque;

use marspot_term::input_core::{LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use marspot_term::render::{InputDisposition, PaneTool, ToolSlot};
use marspot_term::shell_proto::WireSearchHit;

use crate::tools::search_bar::ToolRectPx;

/// §6.5 viewport size.  Header row + this many hit rows + optional
/// footer when more results are loading.
pub const VIEWPORT: usize = 10;
/// Trigger an automatic `SearchMore` request when the visible
/// window's end is within `LOAD_THRESHOLD` rows of the loaded
/// hits' end.
pub const LOAD_THRESHOLD: usize = 8;
/// Hard cap on retained hits (§6.5).  Overflow trims from the front
/// (the oldest results in chronological terms = closer to scrollback
/// floor) so the user always sees the most-recent MAX_HITS.
pub const MAX_HITS: usize = 256;

/// Per-pane search result list state.  Kept generic over the wire
/// shape (we store `WireSearchHit` so C5's main loop can hand it
/// straight to the renderer + L4 wire layer without re-mapping).
#[derive(Clone, Debug)]
pub struct SearchList {
    /// The query that this list belongs to.  Stale batches with a
    /// different `query_id` (D15 race) are dropped on `apply_results`.
    pub query_id: u32,
    /// Loaded hits in **newest-first** order — index 0 is the
    /// newest hit, `hits.len() - 1` the oldest.  `VecDeque` because
    /// MAX_HITS overflow trims from the front (oldest).
    pub hits: VecDeque<WireSearchHit>,
    /// `hits[focused]` is the selected row (renderer shows ▶
    /// marker, highlight overlay binds to its spans).  `None` when
    /// `hits` is empty.
    pub focused: Option<usize>,
    /// `hits[visible_top]` is the topmost rendered row of the
    /// viewport.  Pinned to keep `focused` in view.
    pub visible_top: usize,
    /// True when L3 has signalled "no older results to fetch"
    /// (`has_more = false` on `SearchResults` frame).  Disables
    /// auto-load on scroll.
    pub exhausted_older: bool,
    /// A `SearchMore` is in flight; suppress re-issuing until it
    /// resolves.
    pub pending_more: bool,
    /// Pixel rect for hit-testing mouse clicks.  Set per frame by
    /// the renderer.
    pub rect_px: ToolRectPx,
}

impl Default for SearchList {
    fn default() -> Self {
        Self {
            query_id: 0,
            hits: VecDeque::new(),
            focused: None,
            visible_top: 0,
            exhausted_older: false,
            pending_more: false,
            rect_px: ToolRectPx::default(),
        }
    }
}

/// What `on_scroll` decided.  Drives L2's frame dispatch — when
/// `should_load_more = true`, L2 emits a `SearchMore` frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ScrollOutcome {
    pub redraw: bool,
    pub should_load_more: bool,
}

/// Result of an `on_key` / `on_mouse_click` that may jump the focused
/// pane's view_offset to a hit's row.  C5's L2 main loop honours
/// this by updating `pane.view_offset`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum JumpRequest {
    /// No jump (event didn't request one).
    None,
    /// Caller should jump to the focused hit's primary row (the
    /// renderer will look at `list.focused_hit()` to get the
    /// PhysicalSpan to highlight).
    JumpToFocused,
}

impl SearchList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the currently-focused hit (if any).
    pub fn focused_hit(&self) -> Option<&WireSearchHit> {
        self.focused.and_then(|i| self.hits.get(i))
    }

    /// 1-based focused index for the counter display (`23/412`).
    /// Returns 0 when `focused` is None — caller treats as "no
    /// counter".
    pub fn focused_one_based(&self) -> u32 {
        self.focused.map(|i| (i + 1) as u32).unwrap_or(0)
    }

    /// Apply an incoming batch of `SearchResults`.  Drops stale
    /// batches whose `query_id` doesn't match this list's.  Newer
    /// hits come at the FRONT (newest-first ordering) since the L3
    /// worker emits newest-first.  D-phase SearchMore (older) would
    /// append to the back.
    pub fn apply_results(
        &mut self,
        query_id: u32,
        hits: Vec<WireSearchHit>,
        has_more: bool,
    ) -> bool {
        if query_id != self.query_id {
            // Stale batch — D15 last-write-wins drops it.
            return false;
        }
        let was_empty = self.hits.is_empty();
        for h in hits {
            self.hits.push_back(h);
            if self.hits.len() > MAX_HITS {
                self.hits.pop_front();
                // Adjust indices since we trimmed the front.
                if let Some(f) = self.focused.as_mut() {
                    if *f > 0 {
                        *f -= 1;
                    }
                }
                if self.visible_top > 0 {
                    self.visible_top -= 1;
                }
            }
        }
        if was_empty && !self.hits.is_empty() {
            self.focused = Some(0);
            self.visible_top = 0;
        }
        self.pending_more = false;
        self.exhausted_older = !has_more;
        true
    }

    /// Reset to a clean slate for a fresh query.  C5 calls this when
    /// the bar's debounced query changes.
    pub fn reset_for_query(&mut self, query_id: u32) {
        self.query_id = query_id;
        self.hits.clear();
        self.focused = None;
        self.visible_top = 0;
        self.exhausted_older = false;
        self.pending_more = false;
    }

    /// Move focus forward by 1.  Stops at the end (no wrap-around —
    /// per F1+ user feedback the spec §6.9 wrap behaviour was
    /// disorienting when paging through results; jumping back to
    /// the newest hit after passing the end made the list look like
    /// it "lost" the user's scroll position).  Returns `None` when
    /// already at the end (so the caller can skip the redraw),
    /// `JumpToFocused` when focus moved.  Adjusts `visible_top` to
    /// keep `focused` in the viewport.
    pub fn focus_next(&mut self) -> JumpRequest {
        let n = self.hits.len();
        if n == 0 {
            return JumpRequest::None;
        }
        let cur = self.focused.unwrap_or(0);
        if cur + 1 >= n {
            return JumpRequest::None;
        }
        self.focused = Some(cur + 1);
        self.clamp_visible_to_focus();
        JumpRequest::JumpToFocused
    }

    /// Move focus backward by 1.  Stops at the start (no wrap-around;
    /// see `focus_next`).
    pub fn focus_prev(&mut self) -> JumpRequest {
        let n = self.hits.len();
        if n == 0 {
            return JumpRequest::None;
        }
        let cur = self.focused.unwrap_or(0);
        if cur == 0 {
            return JumpRequest::None;
        }
        self.focused = Some(cur - 1);
        self.clamp_visible_to_focus();
        JumpRequest::JumpToFocused
    }

    fn clamp_visible_to_focus(&mut self) {
        let Some(focused) = self.focused else { return; };
        let vp = VIEWPORT.min(self.hits.len());
        if focused < self.visible_top {
            self.visible_top = focused;
        } else if focused >= self.visible_top + vp {
            self.visible_top = focused + 1 - vp;
        }
    }

    /// Scroll handler — mouse wheel inside the list rect.  `delta`
    /// is in **rows** (positive = scroll down → reveal older = bigger
    /// indices).  Returns redraw + whether to issue a SearchMore.
    pub fn on_scroll(&mut self, delta: i32) -> ScrollOutcome {
        let mut out = ScrollOutcome::default();
        if self.hits.is_empty() {
            return out;
        }
        let max_top = self.hits.len().saturating_sub(VIEWPORT.min(self.hits.len()));
        let new_top = (self.visible_top as i32 + delta).clamp(0, max_top as i32) as usize;
        if new_top != self.visible_top {
            self.visible_top = new_top;
            out.redraw = true;
        }
        if !self.exhausted_older
            && !self.pending_more
            && self.visible_top + VIEWPORT + LOAD_THRESHOLD >= self.hits.len()
        {
            self.pending_more = true;
            out.should_load_more = true;
        }
        out
    }

    /// Click handler.  Caller already hit-tested the list's
    /// `rect_px`.  Maps the click's Y position to a row index using
    /// `cell_h`, focuses it, and returns a JumpRequest.
    pub fn on_mouse_click(&mut self, y_px: f64, cell_h: f64) -> JumpRequest {
        if self.hits.is_empty() || cell_h <= 0.0 {
            return JumpRequest::None;
        }
        // Row 0 of the list rect is the header; rows 1..=VIEWPORT are
        // hit rows; clicking below VIEWPORT is a no-op.
        let local_y = y_px - self.rect_px.y;
        if local_y < cell_h {
            // Header — no jump.
            return JumpRequest::None;
        }
        let row_local = ((local_y - cell_h) / cell_h) as usize;
        if row_local >= VIEWPORT {
            return JumpRequest::None;
        }
        let target = self.visible_top + row_local;
        if target >= self.hits.len() {
            return JumpRequest::None;
        }
        self.focused = Some(target);
        self.clamp_visible_to_focus();
        JumpRequest::JumpToFocused
    }

    /// Whether `(x, y)` lies inside the list's rect.
    pub fn hit_test(&self, x: f64, y: f64) -> bool {
        let r = &self.rect_px;
        if r.w <= 0.0 || r.h <= 0.0 {
            return false;
        }
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
    }

    /// Key handler — Up/Down move focus; Enter requests jump.  Other
    /// keys Pass.  Cmd+G is the bar-level binding (per §6.7); the
    /// `on_key` here only sees ↑/↓/Enter because the keymap routes
    /// other keys elsewhere.
    pub fn handle_key(
        &mut self,
        ev: &MarspotKeyEvent,
        mods: Modifiers,
    ) -> (InputDisposition, JumpRequest) {
        if mods.super_key() || mods.control_key() {
            // Cmd-/Ctrl- combos handled at the pane / app layer.
            return (InputDisposition::Pass, JumpRequest::None);
        }
        match &ev.logical {
            LogicalKey::Named(NamedKey::ArrowUp) => {
                let j = self.focus_prev();
                (InputDisposition::HandledRequestRedraw, j)
            }
            LogicalKey::Named(NamedKey::ArrowDown) => {
                let j = self.focus_next();
                (InputDisposition::HandledRequestRedraw, j)
            }
            LogicalKey::Named(NamedKey::Enter) => {
                if self.focused_hit().is_some() {
                    (
                        InputDisposition::HandledRequestRedraw,
                        JumpRequest::JumpToFocused,
                    )
                } else {
                    (InputDisposition::Pass, JumpRequest::None)
                }
            }
            _ => (InputDisposition::Pass, JumpRequest::None),
        }
    }
}

impl PaneTool for SearchList {
    fn slot(&self) -> ToolSlot {
        ToolSlot::Overlay
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        SearchList::hit_test(self, x, y)
    }
    fn on_key(&mut self, ev: &MarspotKeyEvent, mods: Modifiers) -> InputDisposition {
        self.handle_key(ev, mods).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::input_core::KeyState;
    use marspot_term::shell_proto::WireSearchHit;

    fn hit(line_idx: u64, snippet: &str) -> WireSearchHit {
        WireSearchHit {
            logical_line_idx: line_idx,
            char_offset: 0,
            char_len: snippet.chars().count() as u32,
            snippet: snippet.to_string(),
            snippet_match_start: 0,
            snippet_match_end: snippet.chars().count() as u16,
            spans: Vec::new(),
        }
    }

    fn pressed_named(n: NamedKey) -> MarspotKeyEvent {
        MarspotKeyEvent {
            state: KeyState::Pressed,
            logical: LogicalKey::Named(n),
            text: None,
        }
    }

    #[test]
    fn c3_apply_results_first_batch_focuses_zero() {
        let mut l = SearchList::new();
        l.query_id = 7;
        let ok = l.apply_results(
            7,
            vec![hit(100, "a"), hit(99, "b"), hit(98, "c")],
            true,
        );
        assert!(ok);
        assert_eq!(l.hits.len(), 3);
        assert_eq!(l.focused, Some(0));
        assert_eq!(l.visible_top, 0);
        assert!(!l.exhausted_older);
        assert!(!l.pending_more);
    }

    #[test]
    fn c3_apply_results_stale_qid_dropped() {
        let mut l = SearchList::new();
        l.query_id = 42;
        let ok = l.apply_results(7, vec![hit(1, "x")], false);
        assert!(!ok);
        assert!(l.hits.is_empty());
    }

    #[test]
    fn c3_max_hits_trims_from_front() {
        let mut l = SearchList::new();
        l.query_id = 1;
        // Bulk-load MAX_HITS + 10 → 256 retained, first 10 trimmed.
        let n = MAX_HITS + 10;
        let hits: Vec<WireSearchHit> = (0..n).map(|i| hit(i as u64, "x")).collect();
        l.apply_results(1, hits, false);
        assert_eq!(l.hits.len(), MAX_HITS);
        // Front (newest) starts at logical_line_idx = 10 (we trimmed
        // 10 from the front per overflow rule).
        assert_eq!(l.hits.front().unwrap().logical_line_idx, 10);
        assert_eq!(l.hits.back().unwrap().logical_line_idx, (n - 1) as u64);
    }

    #[test]
    fn c3_focus_next_stops_at_end() {
        // F1+: per user feedback, no wrap-around — staying at the
        // last entry is the more intuitive behaviour when paging.
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(3, "a"), hit(2, "b"), hit(1, "c")], false);
        assert_eq!(l.focused, Some(0));
        assert_eq!(l.focus_next(), JumpRequest::JumpToFocused);
        assert_eq!(l.focused, Some(1));
        l.focus_next();
        assert_eq!(l.focused, Some(2));
        // At end → no-op + JumpRequest::None.
        assert_eq!(l.focus_next(), JumpRequest::None);
        assert_eq!(l.focused, Some(2));
    }

    #[test]
    fn c3_focus_prev_stops_at_start() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(3, "a"), hit(2, "b"), hit(1, "c")], false);
        // At start → no-op + JumpRequest::None.
        assert_eq!(l.focus_prev(), JumpRequest::None);
        assert_eq!(l.focused, Some(0));
        // Move forward, then back, should reach start fine.
        l.focus_next();
        assert_eq!(l.focused, Some(1));
        l.focus_prev();
        assert_eq!(l.focused, Some(0));
        // Hit boundary again.
        assert_eq!(l.focus_prev(), JumpRequest::None);
    }

    #[test]
    fn c3_focus_next_keeps_focused_in_viewport() {
        let mut l = SearchList::new();
        l.query_id = 1;
        let hits: Vec<WireSearchHit> = (0..20).map(|i| hit(i, "x")).collect();
        l.apply_results(1, hits, false);
        // Focus = 0, visible_top = 0.  Press down 11 times → focused
        // = 11, visible_top must have advanced to keep in viewport.
        for _ in 0..11 {
            l.focus_next();
        }
        assert_eq!(l.focused, Some(11));
        assert!(
            l.visible_top + VIEWPORT > l.focused.unwrap(),
            "focused must stay in [visible_top, visible_top+VIEWPORT)"
        );
        assert!(l.focused.unwrap() >= l.visible_top);
    }

    #[test]
    fn c3_scroll_advances_visible_top_and_triggers_more_at_threshold() {
        let mut l = SearchList::new();
        l.query_id = 1;
        let hits: Vec<WireSearchHit> = (0..20).map(|i| hit(i, "x")).collect();
        l.apply_results(1, hits, true); // has_more=true → not exhausted
        let o = l.on_scroll(8); // visible_top: 0→8
        assert!(o.redraw);
        assert_eq!(l.visible_top, 8);
        // visible_top + VIEWPORT + LOAD_THRESHOLD = 8 + 10 + 8 = 26 ≥ 20 → load_more.
        assert!(o.should_load_more);
        assert!(l.pending_more);
        // Second scroll while pending_more shouldn't trigger another load.
        let o2 = l.on_scroll(1);
        assert!(!o2.should_load_more);
    }

    #[test]
    fn c3_scroll_exhausted_does_not_load_more() {
        let mut l = SearchList::new();
        l.query_id = 1;
        let hits: Vec<WireSearchHit> = (0..20).map(|i| hit(i, "x")).collect();
        l.apply_results(1, hits, false); // exhausted
        let o = l.on_scroll(8);
        assert!(o.redraw);
        assert!(!o.should_load_more);
        assert!(!l.pending_more);
    }

    #[test]
    fn c3_mouse_click_on_visible_row_focuses_and_jumps() {
        let mut l = SearchList::new();
        l.query_id = 1;
        let hits: Vec<WireSearchHit> = (0..10).map(|i| hit(i, "x")).collect();
        l.apply_results(1, hits, false);
        l.rect_px = ToolRectPx {
            x: 0.0,
            y: 100.0,
            w: 400.0,
            h: 200.0,
        };
        let cell_h = 16.0;
        // y = 100 (rect top) is header.  y = 100 + cell_h = 116 = row 0.
        // y = 100 + 4 * cell_h = 164 = row 3.
        let j = l.on_mouse_click(164.0, cell_h);
        assert_eq!(j, JumpRequest::JumpToFocused);
        assert_eq!(l.focused, Some(3));
    }

    #[test]
    fn c3_mouse_click_on_header_is_noop() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(0, "x")], false);
        l.rect_px = ToolRectPx {
            x: 0.0,
            y: 100.0,
            w: 400.0,
            h: 200.0,
        };
        let cell_h = 16.0;
        let j = l.on_mouse_click(108.0, cell_h); // y = 108 → header row
        assert_eq!(j, JumpRequest::None);
        assert_eq!(l.focused, Some(0)); // unchanged
    }

    #[test]
    fn c3_arrow_keys_move_focus() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(0, "a"), hit(1, "b"), hit(2, "c")], false);
        let (d, j) = l.handle_key(&pressed_named(NamedKey::ArrowDown), Modifiers::default());
        assert_eq!(d, InputDisposition::HandledRequestRedraw);
        assert_eq!(j, JumpRequest::JumpToFocused);
        assert_eq!(l.focused, Some(1));
        let (_d, j) = l.handle_key(&pressed_named(NamedKey::ArrowUp), Modifiers::default());
        assert_eq!(j, JumpRequest::JumpToFocused);
        assert_eq!(l.focused, Some(0));
    }

    #[test]
    fn c3_enter_jumps_when_focused() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(0, "a")], false);
        let (d, j) = l.handle_key(&pressed_named(NamedKey::Enter), Modifiers::default());
        assert_eq!(d, InputDisposition::HandledRequestRedraw);
        assert_eq!(j, JumpRequest::JumpToFocused);
    }

    #[test]
    fn c3_enter_no_focus_passes_through() {
        let mut l = SearchList::new();
        l.query_id = 1; // no hits applied
        let (d, j) = l.handle_key(&pressed_named(NamedKey::Enter), Modifiers::default());
        assert_eq!(d, InputDisposition::Pass);
        assert_eq!(j, JumpRequest::None);
    }

    #[test]
    fn c3_reset_for_query_clears_state() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(0, "a"), hit(1, "b")], true);
        l.pending_more = true;
        l.exhausted_older = false;
        l.reset_for_query(2);
        assert_eq!(l.query_id, 2);
        assert!(l.hits.is_empty());
        assert_eq!(l.focused, None);
        assert_eq!(l.visible_top, 0);
        assert!(!l.exhausted_older);
        assert!(!l.pending_more);
    }

    #[test]
    fn c3_focused_one_based_counter() {
        let mut l = SearchList::new();
        l.query_id = 1;
        assert_eq!(l.focused_one_based(), 0); // no focus
        l.apply_results(1, vec![hit(0, "a"), hit(1, "b")], false);
        assert_eq!(l.focused_one_based(), 1);
        l.focus_next();
        assert_eq!(l.focused_one_based(), 2);
    }

    #[test]
    fn c3_hit_test_inside_outside() {
        let mut l = SearchList::new();
        l.rect_px = ToolRectPx {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
        };
        assert!(l.hit_test(50.0, 40.0));
        assert!(!l.hit_test(0.0, 40.0));
        assert!(!l.hit_test(50.0, 80.0));
    }

    #[test]
    fn c3_panetool_trait_routes_on_key() {
        let mut l = SearchList::new();
        l.query_id = 1;
        l.apply_results(1, vec![hit(0, "a"), hit(1, "b")], false);
        let dyn_tool: &mut dyn PaneTool = &mut l;
        let d = dyn_tool.on_key(&pressed_named(NamedKey::ArrowDown), Modifiers::default());
        assert_eq!(d, InputDisposition::HandledRequestRedraw);
        assert_eq!(l.focused, Some(1));
    }
}
