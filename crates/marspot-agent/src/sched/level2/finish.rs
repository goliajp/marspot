//! The plan from the schedules found: each switch with its window,
//! kind and gain, when each account empties at the level and at the
//! high band, and the refusals the plan still foresees.

use super::search::Eval;
use super::tally::Stage;
use super::window::{dead_by_gain, kind};
use super::window::{CLEAN_CHANCE, NOTICE_LEAD_H};
use super::{H2, Mode, Pane, Plan, Sched, Switch, Totals};

pub(super) fn plan(e: &mut Eval, best: &[Sched], mode: Mode, active: usize, charge: f64, idle: Totals) -> Plan {
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
            let gain = gain(e, best, p, h, totals_level.money);
            let (input, g) = (e.input(), e.grid());
            let averts = from.is_some_and(|j| e.ctx.deadline[j] < H2);
            // only a first switch at the pane's first allowed moment may
            // open after the latest moment it averts the refusal
            let first = h == 0 && hop.at as usize == g.first[p];
            debug_assert!(first || from.is_none_or(|j| t_open <= g.dead_for(p, j) + 1e-6), "a switch planned after its deadline");
            // a window never closes before it opens: a cut within rounding
            // of the deadline may lie just after it
            let t_dead = match from {
                Some(j) if averts => g.dead_for(p, j).max(t_open),
                _ => dead_by_gain(t_open, gain, pane.rewrite),
            };
            let t_open = if averts { t_open.max(t_dead - lead(pane)) } else { t_open };
            switches.push(Switch {
                sid: pane.sid,
                from: from.map_or(pane.on, |j| input.accounts[j].q.id),
                to: input.accounts[hop.to as usize].q.id,
                order: h as u8,
                t_open,
                t_dead,
                kind: kind(pane, t_open, t_dead),
                exempt: e.exempt(p, s, h),
                gain,
            });
            from = Some(hop.to as usize);
        }
    }
    e.mark(Stage::Finish);
    let empty = level.into_iter().zip(high).map(|(l, h)| [l, h]).collect();
    Plan { mode, switches, scheds: best.to_vec(), deadline: e.ctx.deadline.clone(), totals: totals_level, idle, empty, refusals, active, sims: e.sim.runs, tally: e.tally, charge, cuts: e.grid().cuts.clone() }
}

/// What the plan saves by switch `h` of pane `p` and those after it.
fn gain(e: &mut Eval, best: &[Sched], p: usize, h: usize, money: f64) -> f64 {
    let mut without = best.to_vec();
    without[p].n = h as u8;
    let t = e.sim.run(&e.ctx, &without, 0);
    t.money - money
}

/// How long before its deadline a switch off an account that empties
/// inside the horizon opens: enough for a clean rest at the stopping
/// rule's chance, else the five-minute notice's lead; a drain notice
/// opens at once.
fn lead(p: &Pane) -> f64 {
    if p.clean_per_h > 0.0 {
        -(1.0 - CLEAN_CHANCE).ln() / p.clean_per_h
    } else if p.long_background {
        H2
    } else {
        NOTICE_LEAD_H
    }
}
