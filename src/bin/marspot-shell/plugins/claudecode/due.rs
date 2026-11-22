//! When this plugin's tick is wanted.
//!
//! With claude in no pane and nothing of this plugin's left in flight
//! (no binding, no frozen look, no parked pane, no reclaim run, no
//! monitor, no scan out), a scan finds nothing to bind and reports
//! every pane `Absent`, which it has already reported.  Its one other
//! job is the status-line hook in Claude Code's settings, which the
//! worker looks at again on a change of the switch, on a noted change
//! to an agent's configuration, and otherwise once `RECHECK` has passed.
//! So such a tick is due only when that recheck comes round, when the
//! switch or the noted changes move, or when the panes the last scan saw
//! are not the panes there are; a claude starting in a pane changes what
//! the observer sees, and its knock brings the tick that binds it.

use std::time::{Duration, Instant};

use super::{ClaudecodePlugin, pane_claude};
use crate::plugins::TickDue;

/// The worker's status-line recheck, and the scan interval it was
/// sampled at: a recheck lands within one interval of falling due.
const STATUSLINE_DUE: Duration = Duration::from_secs(60 + 2);

#[derive(Default)]
pub(super) struct Quiet {
    /// When the last scan was asked for, with the switch and the
    /// noted changes as they stood then.
    sent: Option<(Instant, bool, u64)>,
    /// The panes the last scan that came back saw, in its order.
    scanned: Vec<u64>,
}

impl Quiet {
    pub(super) fn sent(&mut self) {
        let look = (marspot::settings::get().cc_statusline_hook, crate::plugins::agents::changes::count());
        self.sent = Some((Instant::now(), look.0, look.1));
    }

    pub(super) fn scanned(&mut self, sessions: impl Iterator<Item = u64>) {
        self.scanned.clear();
        self.scanned.extend(sessions);
    }
}

impl ClaudecodePlugin {
    pub(super) fn due(&self) -> TickDue {
        if !self.initialised {
            return TickDue::Event;
        }
        let in_flight = self.scan_inflight
            || !self.last_mapping.is_empty()
            || !self.last_meta.is_empty()
            || !self.held.is_empty()
            || !self.dormant.is_empty()
            || !self.armed.is_empty()
            || !self.monitors.is_empty()
            || !self.wake_ends.lock().unwrap().is_empty();
        let Some((sent, hook, changes)) = self.quiet.sent.filter(|_| !in_flight) else {
            return TickDue::Cadence;
        };
        if hook != marspot::settings::get().cc_statusline_hook || changes != crate::plugins::agents::changes::count() {
            return TickDue::Cadence;
        }
        let obs = crate::observe::shared().latest();
        let mut scanned = self.quiet.scanned.iter();
        let same_panes = obs.live().all(|s| scanned.next() == Some(&s.entry.id)) && scanned.next().is_none();
        if !same_panes || obs.live().any(|s| pane_claude(s).is_some()) {
            return TickDue::Cadence;
        }
        TickDue::At(sent + STATUSLINE_DUE)
    }
}
