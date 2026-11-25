//! What each claude pane shows about whether a message from marspot may
//! reach it now, for the zombie watch, which runs on a thread of its own
//! and sees none of this plugin's state: the same evidence the idle
//! policy weighs (the pane quiet and waiting for its person, and for how
//! long; the person's cursor), whether the pane's account refuses it,
//! and whether its claude was put to rest.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::ClaudecodePlugin;
use crate::plugins::PluginHost;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneReach {
    pub sid: u64,
    /// The pane's claude, whose inbox a message goes to; 0 for a pane at rest.
    pub claude_pid: i32,
    pub profile: u8,
    /// Since when (unix seconds) the pane has been quiet and waiting
    /// for its person; None while it is not.
    pub awaiting_since: Option<u64>,
    pub user_here: bool,
    /// The account the pane is on refuses it (the pane is stranded).
    pub refused: bool,
    pub dormant: bool,
}

static REACH: Mutex<Vec<PaneReach>> = Mutex::new(Vec::new());

/// Every claude pane's reach as of the last scan.
pub(crate) fn reach() -> Vec<PaneReach> {
    REACH.lock().unwrap().clone()
}

fn publish(panes: Vec<PaneReach>) {
    *REACH.lock().unwrap() = panes;
}

impl ClaudecodePlugin {
    /// After each scan, what other plugins read of it: each pane's
    /// conversation, and its reach.
    pub(super) fn publish_views(&self, host: &dyn PluginHost) {
        super::sessions_view::publish(&self.last_meta);
        self.publish_reach(host);
    }

    /// The bound panes, then the ones at rest.
    fn publish_reach(&self, host: &dyn PluginHost) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut panes: Vec<PaneReach> = self
            .last_meta
            .iter()
            .map(|(sid, m)| {
                let view = host.pane_status(*sid).ok().flatten();
                let waiting = view.filter(|v| v.quiescent && matches!(v.status, marspot::pane_state::PaneStatus::AwaitingUser));
                PaneReach {
                    sid: *sid,
                    claude_pid: m.claude.pid,
                    profile: m.profile_num,
                    awaiting_since: waiting.map(|v| now.saturating_sub(v.held.as_secs())),
                    user_here: self.focused_sid == Some(*sid),
                    refused: self.stranded.contains_key(sid),
                    dormant: self.dormant.iter().any(|d| d.shelld_sid == *sid),
                }
            })
            .collect();
        for d in self.dormant.iter().filter(|d| !self.last_meta.contains_key(&d.shelld_sid)) {
            panes.push(PaneReach { sid: d.shelld_sid, profile: d.profile_num, dormant: true, ..PaneReach::default() });
        }
        panes.sort_by_key(|p| p.sid);
        publish(panes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zombie_reach_is_what_was_last_published() {
        let p = PaneReach { sid: 7, claude_pid: 10, awaiting_since: Some(5), ..PaneReach::default() };
        publish(vec![p.clone()]);
        assert_eq!(reach(), vec![p]);
        publish(Vec::new());
    }
}
