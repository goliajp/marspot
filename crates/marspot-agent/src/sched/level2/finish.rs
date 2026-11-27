//! The plan from the schedules found: each switch with its window,
//! kind and gain, when each account empties at the level and at the
//! high band, and the refusals the plan still foresees.

use super::search::Eval;
use super::window::{dead_by_exhaustion, dead_by_gain, kind};
use super::{H2, Mode, Plan, Sched, Switch, Totals};

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
            let t_dead = match from {
                Some(j) if e.ctx.deadline[j] < H2 => dead_by_exhaustion(e.ctx.deadline[j], high_load(input, g, j)),
                _ => dead_by_gain(t_open, gain, pane.rewrite),
            }
            .max(t_open);
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
    let empty = level.into_iter().zip(high).map(|(l, h)| [l, h]).collect();
    Plan { mode, switches, scheds: best.to_vec(), deadline: e.ctx.deadline.clone(), totals: totals_level, idle, empty, refusals, active, sims: e.sim.runs, charge, cuts: e.grid().cuts.clone() }
}

/// What the plan saves by switch `h` of pane `p` and those after it.
fn gain(e: &mut Eval, best: &[Sched], p: usize, h: usize, money: f64) -> f64 {
    let mut without = best.to_vec();
    without[p].n = h as u8;
    let t = e.sim.run(&e.ctx, &without, 0);
    t.money - money
}

fn high_load(input: &super::Input, g: &super::Grid, j: usize) -> f64 {
    input.panes.iter().enumerate().filter(|(k, _)| g.on[*k] == Some(j)).map(|(_, p)| p.high[0] + p.high[1]).sum()
}
