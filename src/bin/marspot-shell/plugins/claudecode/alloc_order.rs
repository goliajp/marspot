//! The planner's orders, carried out by the main thread, and the one
//! way a live executor changes today's rule.
//!
//! An order is made only while switching is set to live, and only if
//! the pane has not changed since the scan: it works again, the user
//! came to it, a move is on its way, a clean switch now finds work
//! running, it waits on its own timer, the machine is overloaded, or
//! the target refused it.  The pane's state at the start is reported
//! back to the executor, which judges the switch by it.

use marspot::settings::Rebalance;
use marspot_agent::sched::level2::Kind;

use super::allocation_exec;
use super::allocation_view::View;
use super::rebalance::{Pane, Veto};
use super::*;

/// What holds an order back, given the pane as the main thread reads
/// it now; `refused`: the target refused the pane since the feed.
pub(super) fn order_veto(pane: &Pane, kind: Kind, refused: bool) -> Option<Veto> {
    let clean = matches!(kind, Kind::Clean | Kind::Short);
    [
        (pane.move_in_flight, Veto::MoveOnItsWay),
        (!pane.awaiting_user, Veto::Working),
        (pane.user_here, Veto::UserHere),
        (clean && pane.work_in_flight, Veto::WorkInFlight),
        (pane.own_timer, Veto::OwnTimer),
        (pane.host_load.is_some_and(|l| l > marspot::host_load::BUSY), Veto::Overloaded),
        (refused, Veto::RefusedThere),
    ]
    .into_iter()
    .find_map(|(on, v)| on.then_some(v))
}

/// Today's rule still moves panes to save lapsing quota: the planner's
/// executor is not acting, or switching is not set to live.
pub(super) fn waste_is_today_s(plan_live: bool, mode: Rebalance) -> bool {
    !(plan_live && mode == Rebalance::Live)
}

impl ClaudecodePlugin {
    /// Make the switch the planner's executor ordered for `sid`, if any
    /// and if switching is live.  True when the switch was started.
    #[allow(clippy::too_many_arguments)] // one order and the pane's state around it
    pub(super) fn follow_plan(&mut self, host: &dyn PluginHost, view: &mut View, plan: &mut leave_early::Plan, result: &ScanResult, sid: u64, meta: &BindMeta, model: Option<&str>, choice: &CliChoice, in_flight: bool, mode: Rebalance) -> bool {
        let Some(o) = result.plan_orders.iter().find(|o| o.sid == sid).copied() else { return false };
        let from = meta.profile_num;
        if mode != Rebalance::Live || from != o.from {
            return false;
        }
        let status = host.pane_status(o.sid).ok().flatten();
        let pane = self.resting(status.as_ref(), o.sid, meta, result, in_flight, None);
        let refused = plan.rooms().iter().find(|(n, _)| *n == o.to).is_none_or(|(_, r)| r.refused_for(model));
        let line = format!("sid={} P{from} → P{} kind={}", o.sid, o.to, o.kind.word());
        if let Some(v) = order_veto(&pane, o.kind, refused) {
            host.log(LogLevel::Info, "sched.switch.held", &format!("{line}: {}", v.phrase()));
            return false;
        }
        // the transcript is a second witness of a running turn: the veto
        // reads the screen, and a switch the transcript saw mid-turn
        // rolls the executor back
        let turn = !matches!(result.new_activity.get(&o.sid), Some(CcActivity::AwaitingUser));
        let at = rooms::unix_now();
        if !self.start_profile_cycle_to(host, o.sid, meta.clone(), o.to, Some(allocation_exec::say::after_switch(from, o.to)), choice.clone()) {
            return false;
        }
        allocation_exec::started(o.sid, at, turn, pane.work_in_flight);
        host.log(LogLevel::Info, "sched.switch.done", &line);
        self.rebalanced.insert(o.sid, (Instant::now(), o.to));
        plan.place(o.to, model, plan.speed_of(o.sid));
        view.placed(o.sid, o.to);
        self.record_moved(host, o.sid, from, o.to, allocation_view::class_of_judged(model), "plan", None, None);
        true
    }
}

#[cfg(test)]
#[path = "alloc_order_tests.rs"]
mod tests;
