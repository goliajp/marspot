//! What the credits rule reads off a pane before it orders a move: the
//! terminal has gone quiet, codex is not asking for approval, and the
//! input box is empty.  The menu switch trusts peerbus's idle verdict
//! alone; a move nobody asked for also waits for the terminal, so a turn
//! peerbus has not yet seen start is never cut.

use std::time::Duration;

use marspot::pane_state::{BusyKind, PaneStatus};

use crate::plugins::PluginHost;

/// How long the terminal must have been quiet.
const QUIET_FOR: Duration = Duration::from_secs(3);

/// The start of codex's approval question.
const CODEX_ASKS: &str = "Would you like to ";

/// How many non-blank rows from the bottom the question is looked for in.
const ASK_ROWS: usize = 24;

/// How the rule reads a pane: its screen as text, and what its input box
/// holds (`Some("")` empty, None without one).
#[derive(Clone, Copy, Debug)]
pub(super) struct Looks {
    pub screen: fn(u64) -> Option<String>,
    pub composer: fn(u64) -> Option<String>,
}

impl Default for Looks {
    fn default() -> Looks {
        Looks {
            screen: |sid| marspot::pane_read::screen(sid, 0).ok(),
            composer: |sid| marspot::pane_read::composer_box_now(sid).ok().flatten(),
        }
    }
}

impl Looks {
    /// The pane's terminal has been quiet long enough that no turn is
    /// drawing in it, and it is not showing codex's approval prompt.
    pub(super) fn settled(&self, host: &dyn PluginHost, sid: u64) -> bool {
        let quiet = matches!(host.pane_status(sid), Ok(Some(v)) if v.status == PaneStatus::Busy(BusyKind::ForegroundJob) && v.held >= QUIET_FOR);
        quiet && !(self.screen)(sid).is_some_and(|s| asks_approval(&s))
    }

    /// The pane's input box is there and holds nothing.
    pub(super) fn input_empty(&self, sid: u64) -> bool {
        (self.composer)(sid).is_some_and(|t| t.trim().is_empty())
    }
}

/// A row near the bottom of `screen` is codex asking for approval.
pub(super) fn asks_approval(screen: &str) -> bool {
    let rows: Vec<&str> = screen.lines().filter(|r| !r.trim().is_empty()).collect();
    let near = &rows[rows.len().saturating_sub(ASK_ROWS)..];
    near.iter().any(|r| r.trim_start().starts_with(CODEX_ASKS) && r.trim_end().ends_with('?'))
}
