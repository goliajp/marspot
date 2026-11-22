//! A pane's place in its scrollback: moving the viewport, keeping it
//! pinned as rows arrive, and what the scrollbar is drawn from.

use super::Pane;
use crate::render::HistoryView;

impl Pane {
    /// Move THIS pane's viewport, skipping every route that hands the
    /// wheel to the program.  Returns how many lines the viewport
    /// actually moved (positive = further back in history), which is
    /// what a caller extending a selection needs: the wheel may be
    /// clamped by the depth of the scrollback.
    ///
    /// L3 owns its scrollback; L2 has only the visible-window mirror, so
    /// it clamps against the depth the snapshot reported and asks L3 to
    /// publish the new window. In-process backends scroll their own grid.
    pub fn scroll_own_view(&mut self, delta: i32) -> i32 {
        let max = if self.session.is_l3() {
            self.session.l3_scrollback_len() as i32
        } else {
            self.session.grid().scrollback_len() as i32
        };
        let new = (self.view_offset as i32 + delta).clamp(0, max) as u16;
        if new == self.view_offset {
            return 0;
        }
        let moved = new as i32 - self.view_offset as i32;
        self.view_offset = new;
        self.session.forward_scroll(new); // no-op for in-process backends
        moved
    }

    /// Auto-pin the viewport when a row scrolls into scrollback while
    /// the user is viewing history.  L3 runs the symmetric bump in its
    /// publish loop (`marspot-session::main` keeps a `last_scroll_push`
    /// counter), so this side just keeps L2's local `view_offset`
    /// cache in lockstep — no `forward_scroll` because L3 already
    /// published at the bumped offset on its side, saving a round-trip
    /// and the one-frame slip that would otherwise be visible.
    /// No-op when `view_offset == 0` (live tail): nothing to pin.
    pub fn bump_view_offset_on_scroll_push(&mut self, delta: u16) {
        if self.view_offset == 0 || delta == 0 {
            return;
        }
        let max = if self.session.is_l3() {
            self.session.l3_scrollback_len()
        } else {
            self.session.grid().scrollback_len() as u16
        };
        // F1+7 — if `max` (L3-reported scrollback_len) is less than
        // the current `view_offset`, L3 just temporarily lost its
        // scrollback context — most commonly because it entered the
        // alt-screen for a TUI (claudecode etc.) whose own
        // scrollback is shorter than the saved main's history we
        // were viewing.  Clamping view_offset down to `max` would
        // visually look like a "bounce back to live" after every
        // PTY chunk emitted by the TUI.  Leave view_offset as-is
        // until the next jump or snap_to_live moves it intentionally;
        // the renderer will show whatever L3 published last.
        if max < self.view_offset {
            return;
        }
        let new = self.view_offset.saturating_add(delta).min(max);
        self.view_offset = new;
    }

    /// C5 — explicit view_offset setter, used by the search-jump
    /// path which computes the offset analytically (not via
    /// scroll deltas).
    pub fn set_view_offset(&mut self, off: u16) {
        self.view_offset = off;
    }

    /// Force the view back to the live tail. Used by container code
    /// when a non-key event (e.g. PTY write from a remote source)
    /// arrives and we want the user to see fresh output immediately.
    pub fn snap_to_live(&mut self) -> bool {
        if self.view_offset == 0 {
            return false;
        }
        self.view_offset = 0;
        self.session.forward_scroll(0); // no-op for in-process backends
        true
    }

    /// Where the rows on screen sit in the pane's scrollback, what its
    /// scrollbar is drawn from.  An L3 pane shows the window it asked
    /// for whatever its focus, so its offset is the requested one; an
    /// in-process pane shows what it renders at.
    pub fn history_view(&self, focused: bool) -> HistoryView {
        if self.session.is_l3() {
            HistoryView { offset: self.view_offset, len: self.session.l3_scrollback_len() }
        } else {
            let len = self.session.grid().scrollback_len().min(u16::MAX as usize) as u16;
            HistoryView { offset: self.render_view_offset(focused), len }
        }
    }
}
