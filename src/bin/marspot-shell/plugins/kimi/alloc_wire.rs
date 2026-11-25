//! Where the kimi plugin meets the shared allocation: what each pass
//! hands it, and how its answer bends today's choice of account.

use std::path::PathBuf;

use marspot::settings::AgentAllocation;
use marspot_peer::registry::Status;

use super::alloc::{Decision, KimiAlloc, PaneIn, Snap};
use super::extra::{self, Extras};
use super::{accounts, KimiPlugin};
use crate::plugins::allocation::replay::Why;
use crate::plugins::claudecode::quota::{Move, Room};
use crate::plugins::claudecode::with_helpers::HelperRoles;
use crate::plugins::{LogLevel, PluginHost};

/// The accounts a decision reads: as today, with the month and paid
/// use laid over them only when the allocation is live.
pub(super) fn rooms_for(mode: AgentAllocation, mut rooms: Vec<(u8, Room)>, extras: &Extras, now: i64) -> Vec<(u8, Room)> {
    if mode == AgentAllocation::Live {
        extra::mark(&mut rooms, extras, now);
    }
    rooms
}

/// Where a pane that has to move goes: today's `next_move`, whose holds
/// always stand.  Live, a pane whose account the readings say is spent
/// goes to the allocation's target; after a refusal (its turn failed for
/// quota, or a refusal there has not renewed) today's target stands.
pub(super) fn must_move_to(mode: AgentAllocation, today: Move, core: Option<&Decision>, current: u8, refusal: bool) -> Move {
    match core {
        Some(d) if !refusal && matches!(today, Move::To(_)) && mode == AgentAllocation::Live && d.why == Why::Out && d.to != current => Move::To(d.to),
        _ => today,
    }
}

/// The adapter for `mode`: none while off, so nothing is written; a
/// fresh one whenever the mode changes between dryrun and live, since
/// each counts its own rows against the move limits.
pub(super) fn adapter(slot: &mut Option<KimiAlloc>, mode: AgentAllocation, dir: impl FnOnce() -> PathBuf) -> Option<&mut KimiAlloc> {
    let live = match mode {
        AgentAllocation::Off => {
            *slot = None;
            return None;
        }
        AgentAllocation::DryRun => false,
        AgentAllocation::Live => true,
    };
    if slot.as_ref().is_some_and(|a| a.live() != live) {
        *slot = None;
    }
    Some(slot.get_or_insert_with(|| KimiAlloc::new(dir(), live)))
}

impl KimiPlugin {
    /// What the shared allocation decides for each pane this pass, with
    /// its lines written; nothing while `kimi.allocation` is `off`.
    pub(super) fn allocate(&mut self, host: &dyn PluginHost, mode: AgentAllocation, now: i64, roles: &HelperRoles) -> Vec<Decision> {
        if mode == AgentAllocation::Off {
            self.balance.alloc = None;
        }
        if mode == AgentAllocation::Off || self.panes.is_empty() {
            return Vec::new();
        }
        let b = &mut self.balance;
        let mut numbers = b.numbers(None);
        for p in self.panes.values().map(|p| p.profile.unwrap_or(0)) {
            if !numbers.contains(&p) {
                numbers.push(p);
            }
        }
        let mut rooms = accounts::rooms(&numbers, b.usage.as_ref(), &b.refused, now);
        let shut = extra::mark(&mut rooms, &b.extras, now);
        if shut != b.shut_said {
            let words: Vec<String> = shut.iter().map(|(n, why)| format!("P{n}:{}", why.word())).collect();
            host.log(LogLevel::Info, "kimi.alloc.shut", &format!("accounts that take no pane: [{}]", words.join(" ")));
            b.shut_said = shut;
        }
        let panes = self
            .panes
            .iter()
            .map(|(sid, p)| {
                let profile = p.profile.unwrap_or(0);
                // a refusal shuts its account in `rooms` until its window renews
                let out = rooms.iter().find(|(n, _)| *n == profile).is_some_and(|(_, r)| r.refused_for(None));
                let idle = p.facts.as_ref().map(|f| f.status) == Some(Status::Idle);
                PaneIn { sid: *sid, profile, idle, helper: roles.is_helper(*sid), out }
            })
            .collect();
        let feed_at = b.usage.as_ref().map(|u| u.generated_at);
        let snap = Snap { now, feed_at, rooms: &rooms, extras: &b.extras, panes };
        let dir = || super::alloc::events_dir(&marspot::paths::state_root());
        adapter(&mut b.alloc, mode, dir).map_or_else(Vec::new, |a| a.observe(&snap))
    }
}
