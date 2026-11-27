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
        self.expire_prompt_flashes(Instant::now());
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
                // the hidden host is read, so that it is as it would
                // have been when it is shown again, and redraws nothing
                let (n, pushed) = read_frame(p, &frozen);
                let (shown_n, shown_pushed) = match p.peeked() {
                    None if p.peek.is_some() => (0, 0),
                    None => (n, pushed),
                    Some(sid) => match self.shadows.iter_mut().find(|s| s.shelld_session_id() == Some(sid)) {
                        Some(v) => read_frame(v, &frozen),
                        None => (0, 0),
                    },
                };
                if p.peek.is_some() {
                    total += n;
                }
                window_total += shown_n;
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
                    && sel.session_idx == i && shown_pushed > 0 {
                        let bump = shown_pushed as u32;
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
        self.advance_views(Instant::now());
        self.observe_shadow_exits();
        self.close_ended_panes();
        if self.every_session_lost() {
            self.panes_mut().for_each(|p| _ = p.pump());
            self.all_exited = true;
        }
        total
    }

    /// The frames of a shadow nobody views are not read: the pane reads
    /// its newest one when it is viewed or placed into a window.  Only
    /// its session's end is noticed.
    fn observe_shadow_exits(&mut self) {
        for p in self.shadows.iter_mut() {
            p.observe_exit();
        }
    }
}

/// Read pane `p`'s newest frame: whether it took one, and how many rows
/// that pushed into its history.
fn read_frame(p: &mut Pane, frozen: &std::collections::HashSet<u64>) -> (usize, u64) {
    // FREEZE_GRID: skip the pump entirely so the grid the
    // renderer sees stays exactly as it was when the plugin
    // took over.  PTY bytes still queue (shelld + client
    // channel), they get consumed in one shot when the
    // session ends and pump runs again.
    if p.shelld_session_id().is_some_and(|sid| frozen.contains(&sid)) {
        return (0, 0);
    }
    let n = p.pump();
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
        p.bump_view_offset_on_scroll_push(pushed.min(u16::MAX as u64) as u16);
    }
    (n, pushed)
}
