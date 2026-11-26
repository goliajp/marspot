//! When each Claude account's stored resets would be read through the
//! CLI's `/limit-reset` screen: at most once every six hours, and only
//! while every pane on that account waits for its user.
//!
//! Behind `sched.reset_read`, off unless the file says otherwise.  This
//! build opens no pane in any mode: `dryrun` and `live` both log
//! `svc.sched.reset_read` when an account's reading falls due, which is
//! where reading it will start.

use std::collections::HashMap;

use marspot::settings::AgentAllocation;
use marspot_agent::sched::reset::{account_idle, read_due};

use super::{CcActivity, ScanResult};

/// When each account's reading last fell due; one entry per numbered
/// profile, of which there are at most 255.
#[derive(Default)]
pub(super) struct ReadClock {
    last: HashMap<u8, i64>,
}

impl ReadClock {
    /// The accounts whose reading falls due at `now` in `result`, each
    /// counted as read from then.  Nothing while `mode` is off.
    pub(super) fn due(&mut self, result: &ScanResult, mode: AgentAllocation, now: i64) -> Vec<u8> {
        if mode == AgentAllocation::Off {
            return Vec::new();
        }
        self.last.retain(|p, _| result.profiles.iter().any(|(n, _)| n == p));
        let mut due = Vec::new();
        for (profile, _) in &result.profiles {
            let resting = result
                .new_meta
                .iter()
                .filter(|(_, m)| m.profile_num == *profile)
                .map(|(sid, _)| matches!(result.new_activity.get(sid), Some(CcActivity::AwaitingUser)));
            if read_due(self.last.get(profile).copied(), now, account_idle(resting)) {
                self.last.insert(*profile, now);
                due.push(*profile);
            }
        }
        for p in &due {
            marspot::lx_info!("svc.sched.reset_read", &format!("account=P{p} due mode={} opened=no", mode.as_str()));
        }
        due
    }
}
