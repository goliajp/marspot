//! Reading every pane's newest frame once per loop turn.

use super::*;

impl CoreApp {
    /// Drain pending shelld DATA into every pane's grid, keeping
    /// selection state honest (scroll-push bump + in-place-repaint
    /// drop — same contract as src/main.rs `user_event`).
    pub(super) fn pump_all(&mut self) -> usize {
        // C5 — fire any pane's queued SearchScrollback when its
        // debounce window has elapsed.  Cheap when no search is
        // open (single `is_none` check per pane).
        self.process_search_debounces();
        let mut total = 0;
        // Snapshot which session ids are frozen by an L1 PaneSession:
        // the loop holds `&mut` on the window's panes, and reading the
        // map through `self` inside it would overlap that borrow.
        let frozen: std::collections::HashSet<u64> = self
            .pane_sessions
            .iter()
            .filter_map(|(sid, st)| {
                if st.has(marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID) {
                    Some(*sid)
                } else {
                    None
                }
            })
            .collect();
        // Every window's panes, not just the key window's.  A pane
        // does not stop being live because its window lost focus —
        // windows are peers, and a pane skipped here would sit on
        // unread PTY bytes until its window happened to become key.
        for wi in 0..self.windows.len() {
            let mut window_total = 0usize;
            let w = &mut win!(self, wi);
            for (i, p) in w.panes.iter_mut().enumerate() {
                // FREEZE_GRID: skip the pump entirely so the grid the
                // renderer sees stays exactly as it was when the plugin
                // took over.  PTY bytes still queue (shelld + client
                // channel), they get consumed in one shot when the
                // session ends and pump runs again.
                let is_frozen = p
                    .shelld_session_id()
                    .is_some_and(|sid| frozen.contains(&sid));
                if is_frozen {
                    continue;
                }
                let n = p.pump();
                window_total += n;
                let pushed = p.drain_scroll_push_delta();
                // Auto-pin the viewport when a row scrolled into
                // scrollback while the user is reading history.  L3 runs
                // the symmetric bump in its publish loop so view_offset
                // stays consistent across the L2↔L3 boundary without a
                // round-trip.  Without this, every line the shell emits
                // while the user is scrolled back slides the visible
                // content downward by one row — the "老内容被新内容覆盖"
                // symptom.
                if pushed > 0 {
                    let pushed_u16 = pushed.min(u16::MAX as u64) as u16;
                    p.bump_view_offset_on_scroll_push(pushed_u16);
                }
                // Slide the selection anchor + focus when the PTY pushed
                // rows into scrollback so the highlight tracks the same
                // bytes as they roll up.  We do NOT clear the selection
                // just because the PTY autonomously emitted bytes — a
                // claudecode pane prints a spinner every few hundred ms
                // and the old `has_bytes && !dragging → None` rule made
                // selection disappear before the user could Cmd-C.
                // Keyboard input + new clicks + focus changes still clear
                // selection in their own paths; PTY autonomy doesn't.
                if let Some(sel) = w.selection.as_mut()
                    && sel.session_idx == i && pushed > 0 {
                        let bump = pushed as u32;
                        sel.anchor.1 = sel.anchor.1.saturating_add(bump);
                        sel.focus.1 = sel.focus.1.saturating_add(bump);
                    }
            }
            // Only the window that actually took bytes owes a repaint.
            if window_total > 0 {
                w.needs_render = true;
            }
            total += window_total;
            if window_total > 0 {
                self.sync_selection_tape(wi);
            }
        }
        self.pump_shadows(&frozen);
        self.close_ended_panes();
        if self.every_session_lost() {
            self.panes_mut().for_each(|p| _ = p.pump());
            self.all_exited = true;
        }
        total
    }

    /// A shadow is read only to notice its session end: nothing is drawn.
    fn pump_shadows(&mut self, frozen: &std::collections::HashSet<u64>) {
        for p in self.shadows.iter_mut() {
            if !p.shelld_session_id().is_some_and(|sid| frozen.contains(&sid)) {
                p.pump();
            }
        }
    }
}
