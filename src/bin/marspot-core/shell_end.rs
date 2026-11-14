//! What the core does when a pane's session goes away.
//!
//! A shell that ended on its own (`exit`, a command that finished) is
//! the end of that session, so its pane closes exactly as if the user
//! had closed it: the window goes with its last pane, and the app with
//! its last window, through L1, which asked for nothing else.  An L3
//! removes its session dir only in that case, so the dir being gone is
//! how the core tells it from a crash.
//!
//! An L3 that crashed or was killed leaves its pane frozen at the last
//! frame, its dir kept.  When that is true of every pane, the core
//! exits and L1 starts a new one, which resurrects them.

use marspot::lx_event;
use marspot::pane::Pane;

use super::CoreApp;

impl CoreApp {
    /// Close every pane whose shell has ended, highest index first so
    /// the indices still to visit stay valid.
    pub(super) fn close_ended_panes(&mut self) {
        for wi in (0..self.windows.len()).rev() {
            for idx in (0..self.windows[wi].panes.len()).rev() {
                let pane = &self.windows[wi].panes[idx];
                if !pane.shell_ended() {
                    continue;
                }
                lx_event!(
                    "PANE_SHELL_ENDED",
                    "the pane's shell ended; closing the pane",
                    session_id = pane.shelld_session_id().unwrap_or(0),
                    window_id = self.windows[wi].window_id
                );
                self.close_session(wi, idx);
            }
        }
    }

    /// Every pane's session is gone and none of them ended: the core
    /// exits so the next one resurrects them.  Placeholders a closed
    /// pane leaves behind are not sessions, and do not count.
    pub(super) fn every_session_lost(&self) -> bool {
        let mut sessions = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .filter(|p| !p.is_dormant())
            .peekable();
        sessions.peek().is_some() && sessions.all(Pane::is_exited)
    }

    /// No window other than `wi` has a pane that is not a placeholder:
    /// emptying `wi` empties the app.  Windows the closes have emptied
    /// stay in `windows` until L1 closes them.
    pub(super) fn only_window_left(&self, wi: usize) -> bool {
        !self
            .windows
            .iter()
            .enumerate()
            .any(|(j, w)| j != wi && w.panes.iter().any(|p| !p.is_dormant()))
    }
}
