//! Find-in-pane search: Cmd-F, the debounced scrollback queries and the bar's keys.

use super::*;

impl CoreApp {
    /// Minimum pane width (in cell cols) below which Cmd+F is a no-op
    /// per §6.7.  The search bar is 40 cols wide; a 24-col floor leaves
    /// headroom for narrow PaneSession dialogs that legitimately use
    /// the keyboard.
    const SEARCH_MIN_PANE_COLS: u16 = 24;

    /// Cmd+F handler.  When search is closed → open + focus query.
    /// When already open → re-focus + select-all (browser convention
    /// per §6.9).  Narrow-pane fallback skips the open entirely.
    pub(super) fn handle_cmd_f(&mut self, wi: usize) -> bool {
        let idx = win!(self, wi).focused_idx;
        if self.focused_view_ended(wi) {
            return true;
        }
        let Some(pane) = self.shown_pane_mut(wi, idx) else { return false };
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
        let Some((wi, _)) = self.slot_showing(shelld_session_id) else { return };
        let Some(s) = self.pane_by_sid_mut(shelld_session_id).and_then(|p| p.search.as_mut()) else { return };
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
        for pane in self.panes_mut() {
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
            let Some(pane) = self.shown_pane_mut(wi, idx) else { return false };
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

    /// Realise a `JumpRequest::JumpToFocused`: compute view_offset +
    /// HighlightSpan from the focused hit's WireSearchHit, store on
    /// the pane, and forward GridScroll if needed.
    pub(super) fn jump_to_focused_hit(&mut self, wi: usize, pane_idx: usize) {
        let Some(pane) = self.shown_pane_mut(wi, pane_idx) else { return };
        let Some(search) = pane.search.as_ref() else { return };
        let Some(hit) = search.list.focused_hit() else { return };
        let rows = pane.session().grid().rows();
        // Live hit detection: B4 remap stamps live hits with
        // logical_line_idx in [u64::MAX - rows + 1, u64::MAX].  Anything
        // below that threshold is a scrollback hit.
        let live_threshold = u64::MAX.saturating_sub(rows as u64);
        let is_live = hit.logical_line_idx > live_threshold;
        let hit_qid = search.bar.query_id;
        let primary_row = hit
            .spans
            .iter()
            .map(|s| s.phys_row_idx)
            .next()
            .unwrap_or(0);
        let new_spans: Vec<marspot_term::render::HighlightSpan>;
        let new_view_offset: u16;
        if is_live {
            // Live: span.phys_row_idx is already live-local (0..rows-1);
            // view_offset = 0 puts the live grid bottom at viewport row
            // rows-1, so view_row = phys_row_idx works as-is.
            new_view_offset = 0;
            new_spans = hit
                .spans
                .iter()
                .filter(|s| (s.phys_row_idx as u16) < rows)
                .map(|s| marspot_term::render::HighlightSpan {
                    view_row: s.phys_row_idx as u16,
                    col_start: s.col_start,
                    col_end_inclusive: s.col_end_inclusive,
                })
                .collect();
        } else {
            // Scrollback hit: target the hit at viewport row rows/2.
            // At view_offset = K, viewport row R shows
            // scrollback[sb_len - K + R].  Solving for K so that
            // R = target gives K = sb_len + target - primary_row.
            let target_r: i64 = (rows as i64) / 2;
            let sb_len = pane.session().l3_scrollback_len() as i64;
            let raw_k = sb_len + target_r - primary_row as i64;
            // Clamp to a sane view_offset range.
            let max_k = (sb_len + rows as i64 - 1).max(0);
            new_view_offset = raw_k.clamp(0, max_k) as u16;
            new_spans = hit
                .spans
                .iter()
                .map(|s| {
                    // view_row = target_R + (S - primary_row).
                    let vr = target_r + (s.phys_row_idx as i64 - primary_row as i64);
                    (vr, s.col_start, s.col_end_inclusive)
                })
                .filter(|(vr, _, _)| *vr >= 0 && *vr < rows as i64)
                .map(|(vr, cs, ce)| marspot_term::render::HighlightSpan {
                    view_row: vr as u16,
                    col_start: cs,
                    col_end_inclusive: ce,
                })
                .collect();
        }
        pane.active_highlight = Some(marspot_term::render::ActiveHighlight {
            query_id: hit_qid,
            spans: new_spans,
        });
        // F1+4 — set + push without dedup gate.  Previous code
        // checked `old_offset != new_view_offset` and silently
        // skipped forward_scroll when equal, but `pane.view_offset`
        // and the L3Conn-side `req_view_offset` could desync (e.g.
        // L3 republished at 0 after some other path) which left the
        // grid frozen at live even though L2 thought it was at K.
        // Set the L2 side AND push the scroll; the L3Conn does its
        // own req_view_offset dedup that handles the genuinely-no-op
        // case.
        let old_offset = pane.view_offset();
        pane.set_view_offset(new_view_offset);
        pane.session_mut().forward_scroll(new_view_offset);
        lx_event!(
            "L2_SEARCH_JUMP",
            "jump to focused search hit",
            pane_idx = pane_idx as u32,
            is_live = if is_live { 1u32 } else { 0u32 },
            primary_row = primary_row,
            old_offset = old_offset as u32,
            new_offset = new_view_offset as u32
        );
        // F1+5 — bar stays open per §6.7.  Closing on Enter (F1+4)
        // made the immediate next keystroke hit the L3 forward
        // path's snap_to_live and bounce view_offset back to 0
        // before the user could see the hit.  Esc closes the overlay
        // and clears the highlight in one shot.
        win!(self, wi).needs_render = true;
    }
}
