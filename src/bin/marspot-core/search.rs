//! Find-in-pane search: Cmd-F, the debounced scrollback queries and the bar's keys.

use super::*;

impl CoreApp {
    /// Cmd+F handler.  When search is closed → open + focus query.
    /// When already open → re-focus + select-all (browser convention
    /// per §6.9).  Narrow-pane fallback skips the open entirely.
    pub(super) fn handle_cmd_f(&mut self, wi: usize) -> bool {
        let idx = win!(self, wi).focused_idx;
        let Some(pane) = win!(self, wi).panes.get_mut(idx) else { return false };
        // Width gate (§6.7).
        let grid_cols = pane.session().grid().cols();
        if grid_cols < Self::SEARCH_MIN_PANE_COLS {
            lx_event!(
                "L2_SEARCH_NARROW_PANE",
                "Cmd+F suppressed; pane too narrow",
                cols = grid_cols,
                min = Self::SEARCH_MIN_PANE_COLS as u32
            );
            return true; // claim it so the key isn't typed as 'f'
        }
        match pane.search.as_mut() {
            None => {
                pane.search = Some(marspot::pane::PaneSearch::open());
                win!(self, wi).needs_render = true;
                lx_event!(
                    "L2_SEARCH_OPEN",
                    "Cmd+F opened search overlay",
                    pane_idx = idx as u32
                );
            }
            Some(s) => {
                // Re-focus + select-all: bar cursor to end (we don't
                // model selection inside the single-line query; the
                // SearchBar's Cmd+A behaviour maps to "cursor to end").
                s.bar.focused = true;
                s.bar.cursor = s.bar.query_char_len();
                win!(self, wi).needs_render = true;
            }
        }
        true
    }

    /// Apply an incoming `SearchResults` batch.  Routes to the pane
    /// hosting `shelld_session_id`; drops stale batches via the
    /// SearchList's qid check.
    pub(super) fn apply_search_results(
        &mut self,
        shelld_session_id: u64,
        query_id: u32,
        has_more: bool,
        hits: Vec<marspot::shell_proto::WireSearchHit>,
    ) {
        let Some((wi, pi)) = self.find_pane_by_sid(shelld_session_id) else { return };
        let Some(s) = win!(self, wi).panes[pi].search.as_mut() else { return };
        let changed = s.list.apply_results(query_id, hits, has_more);
        if changed {
            win!(self, wi).needs_render = true;
        }
    }

    /// Walk panes whose search is open + debounce_until is past;
    /// emit a fresh `SearchScrollback` frame on each.  Called from
    /// `pump_all` each loop iteration; cheap when no search is open
    /// (idle = single `is_none` check per pane).
    pub(super) fn process_search_debounces(&mut self) {
        let now = std::time::Instant::now();
        for pane in self.windows.iter_mut().flat_map(|w| w.panes.iter_mut()) {
            let Some(s) = pane.search.as_mut() else { continue };
            let fire = matches!(s.bar.debounce_until, Some(t) if now >= t);
            if !fire {
                continue;
            }
            s.bar.debounce_until = None;
            // Skip the emit when the query hasn't actually changed
            // since the last fire (Cmd+A / arrow keys reset the
            // debounce by editing intent but produce no diff).
            if s.bar.query == s.last_emitted_query {
                continue;
            }
            let qid = s.next_query_id;
            s.next_query_id = s.next_query_id.wrapping_add(1).max(1);
            s.bar.query_id = qid;
            s.last_emitted_query = s.bar.query.clone();
            // Empty query — cancel any in-flight worker, clear the
            // list, don't emit a new search.
            if s.bar.query.is_empty() {
                s.list.reset_for_query(qid);
                pane.session_mut().forward_search_cancel(qid);
                pane.active_highlight = None;
                continue;
            }
            s.list.reset_for_query(qid);
            pane.active_highlight = None;
            let case_sensitive = s.bar.case_sensitive;
            let query = s.bar.query.clone();
            pane.session_mut()
                .forward_search_scrollback(qid, case_sensitive, 64, &query);
        }
    }

    /// Routes a key event to the focused pane's search overlay (if
    /// open).  Returns `true` when the key was consumed (don't
    /// propagate to PTY / Cmd-C / etc.).  Implements §6.7 routing
    /// rules: ↑/↓/Enter → list; everything else → bar.
    pub(super) fn search_consume_key(
        &mut self, wi: usize,
        event: &MarspotKeyEvent,
        mods: Modifiers,
    ) -> bool {
        use marspot::input::{LogicalKey, NamedKey};
        let idx = win!(self, wi).focused_idx;
        // Decision: list vs bar.  Done in a scope so the mutable
        // borrow of `win!(self, wi).panes` ends before we call
        // `jump_to_focused_hit(idx)` (which needs a fresh &mut self).
        enum Outcome {
            NotOpen,
            Consumed,
            ConsumedJump,
            ConsumedClosed,
            Pass,
        }
        let outcome: Outcome = {
            let Some(pane) = win!(self, wi).panes.get_mut(idx) else { return false };
            let Some(search) = pane.search.as_mut() else { return false };
            if !search.bar.focused {
                Outcome::NotOpen
            } else {
                let to_list = matches!(
                    event.logical,
                    LogicalKey::Named(NamedKey::ArrowUp)
                        | LogicalKey::Named(NamedKey::ArrowDown)
                        | LogicalKey::Named(NamedKey::Enter)
                );
                if to_list && !search.list.hits.is_empty() {
                    let (disp, jump) = search.list.handle_key(event, mods);
                    let consumed = !matches!(
                        disp,
                        marspot_term::render::InputDisposition::Pass
                    );
                    let jumped = matches!(
                        jump,
                        marspot::tools::search_list::JumpRequest::JumpToFocused
                    );
                    if consumed && jumped {
                        Outcome::ConsumedJump
                    } else if consumed {
                        Outcome::Consumed
                    } else {
                        Outcome::Pass
                    }
                } else {
                    // Bar-routed.
                    let disp = search.bar.handle_key(
                        event,
                        mods,
                        std::time::Instant::now(),
                    );
                    let consumed = !matches!(
                        disp,
                        marspot_term::render::InputDisposition::Pass
                    );
                    if !search.bar.focused {
                        // Esc → close entire overlay.
                        pane.search = None;
                        pane.active_highlight = None;
                        Outcome::ConsumedClosed
                    } else if consumed {
                        Outcome::Consumed
                    } else {
                        Outcome::Pass
                    }
                }
            }
        };
        match outcome {
            Outcome::NotOpen => false,
            Outcome::Pass => false,
            Outcome::Consumed | Outcome::ConsumedClosed => {
                win!(self, wi).needs_render = true;
                true
            }
            Outcome::ConsumedJump => {
                win!(self, wi).needs_render = true;
                self.jump_to_focused_hit(wi, idx);
                true
            }
        }
    }
}
