//! The search for the pane schedules.
//!
//! 1. Start from the standing plan re-timed, or from nobody moving.
//! 2. The active set: panes on an account with an event inside the
//!    horizon (it empties at the high band, its plain use passes its
//!    spare, it renews, it cannot serve them now), panes the start
//!    already moves, and panes for which a proposal (a return to an
//!    account renewing inside the horizon among them) beats staying at
//!    linear prices.
//! 3. Each active pane, dearest to keep first, tries its best linear
//!    proposals that beat staying at linear prices; one is kept only if
//!    the simulated cost falls.
//! 4. When a sweep keeps nothing: pair swaps and two-step chains among
//!    the eight dearest (`moves`).
//! 5. Repeat, at most four rounds.
//!
//! With at most six movable panes of at most four targets each, their
//! joint choices are enumerated instead (`mode=exact`: no schedule of
//! the planner's is cheaper); else, with at most six active panes, theirs
//! alone (`mode=active`: the other panes stay as they are).  The fleet's
//! daily count and the notices an hour are met by a charge per counted
//! switch (`fit`).

use super::exact;
use super::grid::Grid;
use super::moves;
use super::propose::{key, proposals, score};
use super::sim::{Ctx, Sim, Totals};
use super::{EXEMPT_H, H2, Input, MIN_GAIN, MIN_GAIN_EFF, Mode, Plan, Sched, TIE};

const ROUNDS: usize = 4;
const KEEP: usize = 2;
/// A proposal is simulated only when its linear score beats staying by this.
const SCREEN: f64 = 1e-7;
const NOISE: f64 = 1e-9;

#[derive(Clone, Copy, Debug)]
pub(super) struct Cost {
    pub money: f64,
    pub eff: f64,
    pub totals: Totals,
}

pub(super) fn better(a: &Cost, b: &Cost) -> bool {
    if (a.money - b.money).abs() > NOISE { a.money < b.money } else { a.eff < b.eff - NOISE }
}

pub(super) struct Eval<'a, 'b> {
    pub ctx: Ctx<'a, 'b>,
    pub sim: Sim,
    pub charge: f64,
}

impl<'a, 'b> Eval<'a, 'b> {
    pub fn input(&self) -> &'a Input<'b> {
        self.ctx.input
    }

    pub fn grid(&self) -> &'a Grid {
        self.ctx.grid
    }

    pub fn cost(&mut self, s: &[Sched]) -> Cost {
        let totals = self.sim.run(&self.ctx, s, 0);
        Cost { money: totals.money + self.charge * self.counted(s) as f64, eff: totals.eff, totals }
    }

    /// Switch `h` of pane `p` averts a refusal within `EXEMPT_H`.
    pub fn exempt(&self, p: usize, s: &Sched, h: usize) -> bool {
        let g = self.grid();
        let from = if h == 0 { g.on[p] } else { Some(s.hops[h - 1].to as usize) };
        from.is_some_and(|j| self.ctx.deadline[j] < H2 && self.ctx.deadline[j] - g.cuts[s.hops[h].at as usize] <= EXEMPT_H)
    }

    /// Switches that count toward the fleet's day.
    pub fn counted(&self, s: &[Sched]) -> usize {
        s.iter().enumerate().map(|(p, sc)| (0..sc.n as usize).filter(|&h| !self.exempt(p, sc, h)).count()).sum()
    }

    /// Notice-switches inside the first hour.
    fn notices(&self, s: &[Sched]) -> usize {
        let (input, g) = (self.input(), self.grid());
        s.iter()
            .enumerate()
            .map(|(p, sc)| {
                let soon: Vec<bool> = sc.hops().iter().map(|h| g.cuts[h.at as usize] < 1.0).collect();
                super::propose::kinds(input, g, &self.ctx.deadline, p, sc).zip(soon).filter(|(k, soon)| k.notice() && *soon).count()
            })
            .sum()
    }

    pub fn fits(&self, s: &[Sched]) -> bool {
        self.counted(s) <= self.input().fleet_left && self.notices(s) <= self.input().notices_left
    }

    /// Pane `p` following `s` instead of what `cur` has: kept when the
    /// whole plan costs less, or the same within `TIE` and `s` comes
    /// first in the order among equals.
    pub fn try_pane(&mut self, cur: &mut [Sched], cost: &mut Cost, p: usize, s: Sched) -> bool {
        let was = cur[p];
        cur[p] = s;
        let c = self.cost(cur);
        let tie = (c.money - cost.money).abs() <= TIE && (c.eff - cost.eff).abs() <= NOISE;
        let keep = if tie { key(self.input(), &s) < key(self.input(), &was) } else { better(&c, cost) };
        if keep {
            *cost = c;
        } else {
            cur[p] = was;
        }
        keep
    }
}

pub(super) fn run(input: &Input, start: Option<&[Sched]>) -> Plan {
    let g = Grid::new(input);
    let ctx = Ctx::new(input, &g);
    let sim = Sim::new(&ctx);
    let mut e = Eval { ctx, sim, charge: 0.0 };
    let n = input.panes.len();
    let idle = vec![Sched::STAY; n];
    let start: Vec<Sched> = start.filter(|s| s.len() == n).map(<[Sched]>::to_vec).unwrap_or_else(|| idle.clone());
    let (mut best, mut mode, active) = solve(&mut e, &start);
    if !e.fits(&best) {
        (best, mode) = fit(&mut e, best, mode);
    }
    let charge = e.charge;
    e.charge = 0.0;
    prune(&mut e, &mut best);
    let idle_cost = e.cost(&idle);
    let c = e.cost(&best);
    let gains = idle_cost.money - c.money >= MIN_GAIN || (c.money <= idle_cost.money + NOISE && idle_cost.eff - c.eff >= MIN_GAIN_EFF);
    if !gains {
        best = idle;
    }
    super::finish::plan(&mut e, &best, mode, active, charge, idle_cost.totals)
}

/// The plan that fits the fleet's day and the notices an hour.  The
/// charge per counted switch is the gain of the dearest pane left out:
/// each pane's switches are worth what the plan loses without them, the
/// panes of least gain per counted switch are left out until the plan
/// fits, and the search runs once more from there at that charge, so a
/// switch comes back only if it gains more than the charge.  With
/// separable gains this is the charge bisection would settle on.
fn fit(e: &mut Eval, over: Vec<Sched>, mode: Mode) -> (Vec<Sched>, Mode) {
    let base = e.cost(&over).money;
    let mut gains: Vec<(f64, usize)> = Vec::new();
    for p in 0..over.len() {
        let counted = (0..over[p].n as usize).filter(|&h| !e.exempt(p, &over[p], h)).count();
        if counted == 0 {
            continue;
        }
        let mut t = over.clone();
        t[p] = Sched::STAY;
        gains.push(((e.cost(&t).money - base) / counted as f64, p));
    }
    gains.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut trimmed = over;
    let mut charge = 0.0_f64;
    for (g, p) in gains {
        if e.fits(&trimmed) {
            break;
        }
        trimmed[p] = Sched::STAY;
        charge = charge.max(g);
    }
    e.charge = charge.max(0.0) + NOISE;
    let (s, m, _) = solve(e, &trimmed);
    if e.fits(&s) { (s, m) } else { (moves::trim(e, trimmed), mode) }
}

/// A pane whose switches the plan does no worse without stays: a chain
/// or a swap can leave a switch that no longer gains anything.
fn prune(e: &mut Eval, best: &mut [Sched]) {
    let mut cur = e.cost(best);
    for p in 0..best.len() {
        if best[p].n == 0 {
            continue;
        }
        let was = best[p];
        best[p] = Sched::STAY;
        let c = e.cost(best);
        if better(&cur, &c) {
            best[p] = was;
        } else {
            cur = c;
        }
    }
}

/// The best schedules from `start` at the charge in force.
fn solve(e: &mut Eval, start: &[Sched]) -> (Vec<Sched>, Mode, usize) {
    let mut cur = start.to_vec();
    let mut cost = e.cost(&cur);
    let active = active(e, &cur);
    let movable: Vec<usize> = (0..cur.len()).filter(|&p| e.grid().movable(e.input(), p)).collect();
    if let Some(found) = exact::run(e, &cur, &movable) {
        let c = e.cost(&found);
        if better(&c, &cost) {
            cur = found;
        }
        return (cur, Mode::Exact, active.len());
    }
    if let Some(found) = exact::run(e, &cur, &active) {
        let c = e.cost(&found);
        if better(&c, &cost) {
            cur = found;
        }
        return (cur, Mode::Active, active.len());
    }
    for _ in 0..ROUNDS {
        let mut kept = false;
        let order = by_stay_cost(e, &cur, &active);
        // proposals are priced off the last run, which may be a trial
        // that differs from `cur` in one pane: close enough to rank by
        for &p in &order {
            let here = score(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, &cur[p]);
            let mut props = proposals(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, KEEP);
            if cur[p] != Sched::STAY {
                props.push((score(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, &Sched::STAY), Sched::STAY));
            }
            for (sc, s) in props {
                if sc < here - SCREEN {
                    kept |= e.try_pane(&mut cur, &mut cost, p, s);
                }
            }
        }
        if !kept {
            let dear: Vec<usize> = order.iter().copied().take(8).collect();
            kept = moves::pairs_and_chains(e, &mut cur, &mut cost, &dear);
        }
        if !kept {
            break;
        }
    }
    (cur, Mode::Search, active.len())
}

/// Panes dearest to keep where they are first: their linear score.
fn by_stay_cost(e: &mut Eval, cur: &[Sched], active: &[usize]) -> Vec<usize> {
    e.cost(cur);
    let mut v: Vec<(f64, usize)> = active.iter().map(|&p| (score(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, &cur[p]), p)).collect();
    v.sort_by(|a, b| b.0.total_cmp(&a.0).then(e.input().panes[a.1].sid.cmp(&e.input().panes[b.1].sid)));
    v.into_iter().map(|x| x.1).collect()
}

/// Panes on an account with an event inside the horizon (it empties at
/// the high band, its plain use passes its spare, it renews, it cannot
/// serve them now), panes the start moves or a run refuses, and panes a
/// proposal beats staying for at linear prices.
fn active(e: &mut Eval, cur: &[Sched]) -> Vec<usize> {
    e.cost(cur);
    let (input, g) = (e.input(), e.grid());
    let event: Vec<bool> = (0..input.accounts.len())
        .map(|j| e.ctx.deadline[j] < H2 || e.sim.plain[j] > input.accounts[j].spare || !input.accounts[j].q.open || e.sim.empty[j].is_some() || g.renews[j].is_some())
        .collect();
    let beaten = |p: usize| {
        let here = score(input, g, &e.sim, &e.ctx.deadline, p, &cur[p]);
        proposals(input, g, &e.sim, &e.ctx.deadline, p, 1).first().is_some_and(|x| x.0 < here - SCREEN)
    };
    (0..input.panes.len())
        .filter(|&p| g.movable(input, p) || g.on[p].is_none())
        .filter(|&p| cur[p].n > 0 || g.on[p].is_none_or(|j| event[j]) || e.sim.refused.iter().any(|r| r.0 == p) || beaten(p))
        .collect()
}

/// The plan of exactly `scheds`, costed and timed, with no search.
pub(super) fn evaluate(input: &Input, scheds: &[Sched]) -> Plan {
    evaluate_on(input, &Grid::new(input), scheds)
}

/// As `evaluate`, on the grid `g` of `input` already worked out.
pub(super) fn evaluate_on(input: &Input, g: &Grid, scheds: &[Sched]) -> Plan {
    let ctx = Ctx::new(input, g);
    let sim = Sim::new(&ctx);
    let mut e = Eval { ctx, sim, charge: 0.0 };
    let idle = e.cost(&vec![Sched::STAY; input.panes.len()]).totals;
    super::finish::plan(&mut e, scheds, Mode::Search, 0, 0.0, idle)
}
