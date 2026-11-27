//! What a pane asks of its session's history beyond a scroll: a search
//! of it, and a jump to the prompt before or after the view, whose row
//! is then marked for a moment.

use std::time::{Duration, Instant};

use super::{L3Conn, Pane, PaneBackend};
use crate::render::HighlightSpan;
use crate::shell_proto::{
    encode_grid_scroll, encode_prompt_jump, encode_search_cancel, encode_search_more, encode_search_scrollback,
    Frame, MsgType, PromptDir, PromptJump, PromptJumped,
};

/// How long the row a jump put on top stays marked.
pub const PROMPT_FLASH: Duration = Duration::from_millis(600);

/// The row a jump put on top, marked until `until`.
#[derive(Clone, Copy, Debug)]
pub struct PromptFlash {
    span: HighlightSpan,
    until: Instant,
}

impl PaneBackend {
    /// C5 — send a `SearchScrollback` frame to the L3 session
    /// behind this pane.  No-op on non-L3 backends (search is
    /// File-backed scrollback only, which only L3 owns).
    pub fn forward_search_scrollback(&mut self, query_id: u32, case_sensitive: bool, max_total: u32, query: &str) {
        if let PaneBackend::L3(c) = self {
            let payload = encode_search_scrollback(query_id, case_sensitive, max_total, query);
            let _ = c.control.send(Frame::new(MsgType::SearchScrollback, payload));
        }
    }

    /// C5 — cancel an in-flight L3 search by query_id.
    pub fn forward_search_cancel(&mut self, query_id: u32) {
        if let PaneBackend::L3(c) = self {
            let _ = c.control.send(Frame::new(MsgType::SearchCancel, encode_search_cancel(query_id)));
        }
    }

    /// C5 — request more hits for the in-flight search (D-phase
    /// pagination; v1 only uses direction=0 → older).
    pub fn forward_search_more(&mut self, query_id: u32, count: u32) {
        if let PaneBackend::L3(c) = self {
            let _ = c.control.send(Frame::new(MsgType::SearchMore, encode_search_more(query_id, count, 0)));
        }
    }
}

impl L3Conn {
    /// Ask L3 to publish the window at `view_offset` rows up from live
    /// (dedup'd against the last requested offset).  Best-effort over the
    /// control socket; the new window arrives on the next `poll`.  Every
    /// ask, sent or not, makes the answer to an earlier prompt jump stale:
    /// the session applies the two in order and ends where this one says.
    pub(super) fn forward_scroll(&mut self, view_offset: u16) {
        self.view_seq = self.view_seq.wrapping_add(1);
        if self.req_view_offset == Some(view_offset) {
            return;
        }
        self.req_view_offset = Some(view_offset);
        let _ = self.control.send(Frame::new(MsgType::GridScroll, encode_grid_scroll(view_offset)));
    }

    /// Ask the session to move the view to the prompt `dir` of it.  Until
    /// it answers, where it publishes is unknown here, so the next scroll
    /// is sent even to the offset last asked for.
    fn forward_prompt_jump(&mut self, dir: PromptDir) {
        self.req_view_offset = None;
        let jump = PromptJump { dir, seq: self.view_seq };
        let _ = self.control.send_with(MsgType::PromptJump, |out| encode_prompt_jump(jump, out));
    }
}

impl Pane {
    /// Cmd-↑ / Cmd-↓: ask the session for the prompt `dir` of the view.
    /// The view moves when the session answers; a session with no
    /// prompt that way, or an in-process pane, does nothing.
    pub fn jump_to_prompt(&mut self, dir: PromptDir) {
        if let PaneBackend::L3(c) = &mut self.session {
            c.forward_prompt_jump(dir);
        }
    }

    /// The session moved the view to a prompt: this side takes the offset
    /// it moved to and marks the prompt's row until `now + PROMPT_FLASH`.
    /// An answer to a jump asked before a later key or scroll is dropped:
    /// the session has already moved on to where that one said.  True
    /// when the answer was taken.
    pub fn prompt_jumped(&mut self, said: PromptJumped, now: Instant) -> bool {
        if let PaneBackend::L3(c) = &mut self.session {
            if said.seq.is_some_and(|s| s != c.view_seq) {
                return false;
            }
            // where the session is, so a scroll to there is not sent
            c.req_view_offset = Some(said.view_offset);
        }
        self.view_offset = said.view_offset;
        let span = HighlightSpan { view_row: said.row, col_start: 0, col_end_inclusive: u16::MAX };
        self.prompt_flash = Some(PromptFlash { span, until: now + PROMPT_FLASH });
        true
    }

    /// When the marked row is to be unmarked, while it is.
    pub fn prompt_flash_until(&self) -> Option<Instant> {
        self.prompt_flash.map(|f| f.until)
    }

    /// Unmark the row a jump marked, if its time is up; true when it was.
    pub fn expire_prompt_flash(&mut self, now: Instant) -> bool {
        let over = self.prompt_flash.is_some_and(|f| f.until <= now);
        if over {
            self.prompt_flash = None;
        }
        over
    }

    /// The rows the renderer marks: the jump's while it lasts, else the
    /// search hit's.
    pub(super) fn highlight_spans(&self) -> &[HighlightSpan] {
        match &self.prompt_flash {
            Some(f) => std::slice::from_ref(&f.span),
            None => self.active_highlight.as_ref().map(|h| h.spans.as_slice()).unwrap_or(&[]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(view_offset: u16, row: u16) -> PromptJumped {
        PromptJumped { view_offset, row, seq: Some(0) }
    }

    /// The row on top is marked for its time and then not, and the
    /// search's mark is what shows again.
    #[test]
    fn a_jump_marks_the_top_row_until_its_time_is_up() {
        let mut p = Pane::new_vacant(1, 80, 24);
        let now = Instant::now();
        assert!(p.prompt_jumped(to(30, 0), now));
        assert_eq!(p.view_offset, 30);
        assert_eq!(p.highlight_spans(), &[HighlightSpan { view_row: 0, col_start: 0, col_end_inclusive: u16::MAX }]);
        assert_eq!(p.prompt_flash_until(), Some(now + PROMPT_FLASH), "the loop has a deadline to wake at");
        assert!(!p.expire_prompt_flash(now + PROMPT_FLASH / 2));
        assert!(p.expire_prompt_flash(now + PROMPT_FLASH));
        assert!(p.highlight_spans().is_empty());
        assert_eq!(p.prompt_flash_until(), None, "nothing left to wake for");
    }

    /// Going back to the live screen, or scrolling by hand, unmarks it.
    #[test]
    fn leaving_the_view_unmarks_the_row() {
        let mut p = Pane::new_vacant(1, 80, 24);
        p.prompt_jumped(to(30, 0), Instant::now());
        assert!(p.snap_to_live());
        assert!(p.highlight_spans().is_empty());
        p.prompt_jumped(to(30, 0), Instant::now());
        p.set_view_offset(12);
        assert!(p.highlight_spans().is_empty());
    }

    /// A jump down to a prompt on the live screen marks the row the
    /// prompt is on there, not the top row.
    #[test]
    fn a_jump_to_the_live_screen_marks_the_prompt_row() {
        let mut p = Pane::new_vacant(1, 80, 24);
        p.prompt_jumped(to(0, 17), Instant::now());
        assert_eq!(p.view_offset, 0);
        assert_eq!(p.highlight_spans(), &[HighlightSpan { view_row: 17, col_start: 0, col_end_inclusive: u16::MAX }]);
    }
}
