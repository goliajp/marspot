//! Where a refused pane goes, or why it stays.
//!
//! When it waits, how long and how often it moves are `hold_for`'s.
//! Where it goes is the allocation core's rule (a) for a pane of its
//! own; a helper keeps the choice it had before the core chose for
//! panes: `next_move`'s ranking over the rooms that last it, then the
//! roomiest account, so helpers refused one after another do not all
//! land on the account that renews first.

use super::super::allocation_view::{HELD_BACK, View};
use super::super::*;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::replay::Inputs;

/// What the sweep read about a refused pane, for the choice.
pub(in super::super) struct Refused<'a> {
    pub sid: u64,
    pub meta: &'a BindMeta,
    pub needs_model: Option<&'a str>,
    pub class: Class,
    pub helper: bool,
    pub tool_executing: bool,
    pub work_running: bool,
    pub warned: bool,
}

impl ClaudecodePlugin {
    /// The move for a refused pane, with the core's inputs when the core
    /// chose it.
    pub(super) fn refused_choice(&self, plan: &leave_early::Plan, view: &View, result: &ScanResult, r: &Refused) -> (quota::Move, Option<Inputs>) {
        let (sid, from) = (r.sid, r.meta.profile_num);
        let all = plan.rooms().to_vec();
        let rooms = if r.helper { plan.rooms_for_helper(from, r.needs_model, plan.speed_of(sid)) } else { all };
        let tally = self.stranded.get(&sid);
        let situation = quota::Situation {
            rooms: &rooms,
            current: from,
            needs_model: r.needs_model,
            tool_executing: r.tool_executing,
            work_running: r.work_running,
            moves_this_episode: tally.map_or(0, |s| s.moves),
            last_target: tally.and_then(|s| s.last_target),
            stranded_for: tally.map(|s| s.since.elapsed()),
            since_last_move: self.moved_at.get(&sid).map(|at| at.elapsed()),
            warned: r.warned,
            background_refused: result.agent_refused.get(&sid),
        };
        if r.helper {
            let feed_at = result.usage.as_ref().map_or(0, |u| u.generated_at);
            let decision = quota::next_move(&situation);
            return (quota::roomiest::helper_move(quota::roomiest::Pool::Claude, feed_at, decision, &rooms, from, r.needs_model), None);
        }
        match quota::hold_for(&situation) {
            Some(h) => (h, None),
            None => view.must_move(&rooms, sid, from, r.class),
        }
    }

    /// Mark a pane held back for the Fable panes, or no longer held
    /// back, and log the start of each hold.  The session is not told:
    /// a line in its inbox starts a turn on the account that refuses it.
    pub(super) fn note_held_back(&mut self, host: &dyn PluginHost, sid: u64, meta: &BindMeta, why: Option<&str>) {
        let Some(tally) = self.stranded.get_mut(&sid) else { return };
        let held = why == Some(HELD_BACK);
        if held && !tally.held_back {
            host.log(LogLevel::Info, "quota.held_back_noted", &format!("sid={sid} on P{}: it waits for the Fable panes", meta.profile_num));
        }
        tally.held_back = held;
    }
}
