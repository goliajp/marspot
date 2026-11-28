//! The plan from the schedules found: each switch with its window,
//! kind and gain, when each account empties at the level and at the
//! high band, and the refusals the plan still foresees.

use super::search::Eval;
use super::tally::Stage;
use super::window::{dead_by_gain, kind_of_window};
use super::window::{CLEAN_CHANCE, NOTICE_LEAD_H};
use super::{H2, Mode, Pane, Plan, Sched, Switch, Totals};

/// `kept`: switches of the standing plan whose window is not open yet,
/// `(sid, order, gain, gain_eff)`; one that averts a refusal keeps that
/// gain, since its window does not depend on it and nothing reads it
/// before it opens.  Such a switch is marked `carried`: its gain is the
/// one stored, not this pass's.
pub(super) fn plan(e: &mut Eval, best: &[Sched], mode: Mode, active: usize, charge: f64, idle: Totals, kept: &[(u64, u8, f64, f64)]) -> Plan {
    e.sim.run(&e.ctx, best, 1);
    let high: Vec<Option<f64>> = e.sim.empty.clone();
    let totals_level = e.sim.run(&e.ctx, best, 0);
    let level: Vec<Option<f64>> = e.sim.empty.clone();
    let (input, g) = (e.input(), e.grid());
    let refusals = e.sim.refused.iter().map(|&(p, at)| (input.panes[p].sid, at)).collect();
    let mut switches = Vec::new();
    for (p, s) in best.iter().enumerate() {
        let pane = &input.panes[p];
        let mut from = g.on[p];
        for (h, hop) in s.hops().iter().enumerate() {
            let t_open = g.cuts[hop.at as usize];
            let averts = from.is_some_and(|j| e.ctx.deadline[j] < H2);
            let old = kept.iter().find(|k| (k.0, usize::from(k.1)) == (pane.sid, h)).filter(|_| averts);
            let (gain, gain_eff) = match old {
                Some(k) => (k.2, k.3),
                None => gain(e, best, p, h, &totals_level),
            };
            let (input, g) = (e.input(), e.grid());
            // only a first switch at the pane's first allowed moment may
            // open after the latest moment it averts the refusal
            let first = h == 0 && hop.at as usize == g.first[p];
            debug_assert!(first || from.is_none_or(|j| t_open <= g.dead_for(p, j) + 1e-6), "a switch planned after its deadline");
            // a window never closes before it opens: a cut within rounding
            // of the deadline may lie just after it
            // a move onto an account before its renewal uses up what lapses
            // there: it is worth its time only until the renewal, and is not
            // put off toward the source's deadline.  That holds for a move off
            // an account that empties too: it opens when planned and closes
            // at the deadline or the renewal, whichever is first; opened a
            // lead before the deadline instead, the closed loop refused more
            // panes and spent more
            let drains = g.renews[hop.to as usize].filter(|&r| (hop.at as usize) < r).map(|r| g.cuts[r]);
            let t_dead = match from {
                Some(j) if averts => g.dead_for(p, j).min(drains.unwrap_or(f64::INFINITY)).max(t_open),
                _ => dead_by_gain(t_open, gain, pane.rewrite).min(drains.unwrap_or(f64::INFINITY)).max(t_open),
            };
            // a switch planned at the source's latest moment (a keeper's)
            // opens the same lead before it as one planned earlier, from the
            // pane's own earliest moment, wherever it goes
            let at_late = from.and_then(|j| g.late[j]) == Some(hop.at as usize);
            let late = averts && (drains.is_none() || at_late);
            // the window's length is taken as worked out: `t_dead` less the
            // opening moved up to it differs from the lead by rounding, and
            // a clean rest's chance taken on that falls under the stopping
            // rule's for about half of them
            let floor = if late { t_open.min(pane.earliest_h.max(0.0)) } else { t_open };
            let w = if late { (t_dead - floor).min(lead(pane)) } else { t_dead - t_open };
            let t_open = if late { floor.max(t_dead - lead(pane)) } else { t_open };
            switches.push(Switch {
                sid: pane.sid,
                from: from.map_or(pane.on, |j| input.accounts[j].q.id),
                to: input.accounts[hop.to as usize].q.id,
                order: h as u8,
                t_open,
                t_dead,
                kind: kind_of_window(pane, w),
                exempt: e.exempt(p, s, h),
                gain,
                gain_eff,
                carried: old.is_some(),
            });
            from = Some(hop.to as usize);
        }
    }
    e.mark(Stage::Finish);
    let empty = level.into_iter().zip(high).map(|(l, h)| [l, h]).collect();
    Plan { mode, switches, scheds: best.to_vec(), deadline: e.ctx.deadline.clone(), totals: totals_level, idle, empty, refusals, active, sims: e.sim.runs, tally: e.tally, charge, cuts: e.grid().cuts.clone() }
}

/// What the plan saves by switch `h` of pane `p` and those after it:
/// money and stall hours.
fn gain(e: &mut Eval, best: &[Sched], p: usize, h: usize, with: &Totals) -> (f64, f64) {
    let mut without = best.to_vec();
    without[p].n = h as u8;
    let t = e.sim.run(&e.ctx, &without, 0);
    (t.money - with.money, t.eff - with.eff)
}

/// How long before its deadline a switch off an account that empties
/// inside the horizon opens: enough for a clean rest at the stopping
/// rule's chance, else the five-minute notice's lead; a drain notice
/// opens at once.
fn lead(p: &Pane) -> f64 {
    if p.clean_per_h > 0.0 {
        -crate::detmath::ln(1.0 - CLEAN_CHANCE) / p.clean_per_h
    } else if p.long_background {
        H2
    } else {
        NOTICE_LEAD_H
    }
}
