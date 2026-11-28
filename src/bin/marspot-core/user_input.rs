//! Telling the shell that the user put input into a pane other than by
//! a key: a file dropped on it, the mouse or the wheel reported to its
//! program.  The shell sees keys itself; these it would not, and a
//! scripted run that must find the pane untouched has to know of them.

use std::time::{Duration, Instant};

use super::*;

/// Between two reports for the same pane: a wheel or a drag reports
/// many times a second, and one report says all the shell needs.
const EVERY: Duration = Duration::from_secs(1);

/// The last pane reported, and when.
#[derive(Default)]
pub(super) struct Told(Option<(u64, Instant)>);

impl CoreApp {
    /// The user put input into session `sid`.
    pub(super) fn user_wrote(&mut self, sid: u64) {
        let now = Instant::now();
        if self.user_told.0.is_some_and(|(s, at)| s == sid && now.duration_since(at) < EVERY) {
            return;
        }
        self.user_told.0 = Some((sid, now));
        self.pending_to_shell.push((MsgType::PaneUserInput, marspot::shell_proto::encode_pane_user_input(sid)));
    }

    /// As `user_wrote`, for what slot `idx` of window `wi` shows.
    pub(super) fn user_wrote_into(&mut self, wi: usize, idx: usize) {
        if let Some(sid) = self.shown_pane(wi, idx).and_then(|p| p.shelld_session_id()) {
            self.user_wrote(sid);
        }
    }
}

#[cfg(test)]
#[path = "user_input_tests.rs"]
mod tests;
