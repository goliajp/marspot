//! Where a refused pane goes, or why it stays.
//!
//! When it waits, how long and how often it moves are `hold_for`'s.
//! Where it goes is the allocation core's: rule (a) for a pane of its
//! own, rule (c) for a helper, with the helpers placed since the feed
//! was written counted where they went, so helpers refused one after
//! another do not all land on the same account.

use super::super::allocation_view::View;
use super::super::*;
use super::super::quota::helper_pick::{Pool, placed};
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
        let rooms = plan.rooms().to_vec();
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
        if let Some(h) = quota::hold_for(&situation) {
            return (h, None);
        }
        if !r.helper {
            let today = view.must_move(&rooms, sid, from, r.class);
            let to = if let quota::Move::To(t) = today.0 { Some(t) } else { None };
            return match view.routed(&rooms, "refused", sid, from, r.class, r.needs_model, to) {
                Some(m) => (m, None),
                None => today,
            };
        }
        placed(Pool::Claude, |p| {
            let now = crate::plugins::claudecode::rooms::unix_now();
            p.settle(&view.helpers(), now);
            let chosen = view.helper_must_move(&rooms, sid, from, r.class, p);
            if let quota::Move::To(to) = chosen.0 {
                p.note(to, r.class, now);
            }
            chosen
        })
    }
}
