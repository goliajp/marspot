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
//! 6. Each of the eight dearest active panes tries every schedule it may
//!    hold once against the result.
//!
//! With at most six movable panes of at most four targets each, their
//! joint choices are enumerated instead (`mode=exact`: no schedule of
//! the planner's is cheaper); else, with at most six active panes, theirs
//! alone (`mode=active`: the other panes stay as they are).  The fleet's
//! daily count and the notices an hour are met by a charge per counted
//! switch (`fit`).

use super::exact;
use super::fit::{fit, prune};
use super::grid::Grid;
use super::moves;
use super::propose::{allowed, candidates, key, proposals, score};
use super::ctx::Ctx;
use super::sim::Sim;
use super::tally::{Stage, Tally};
use super::{EXEMPT_H, H2, Input, MIN_GAIN, MIN_GAIN_EFF, Mode, Plan, Sched, TIE, Totals};

const ROUNDS: usize = 4;
const KEEP: usize = 2;
/// Panes whose every schedule is tried once at the end of a search.
const POLISH: usize = 8;
/// A proposal is simulated only when its linear score beats staying by this.
const SCREEN: f64 = 1e-7;
pub(super) const NOISE: f64 = 1e-9;

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
    pub tally: Tally,
}

impl<'a, 'b> Eval<'a, 'b> {
    pub fn input(&self) -> &'a Input<'b> {
        self.ctx.input
    }

    pub fn grid(&self) -> &'a Grid {
        self.ctx.grid
    }

    /// The simulations since the last mark were run for `stage`.
    pub fn mark(&mut self, stage: Stage) {
        self.tally.mark(stage, self.sim.runs);
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
        s.iter().enumerate().map(|(p, sc)| self.counted_of(p, sc)).sum()
    }

    /// Switches of pane `p` following `s` that count toward the fleet's day.
    fn counted_of(&self, p: usize, s: &Sched) -> usize {
        (0..s.n as usize).filter(|&h| !self.exempt(p, s, h)).count()
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
    let mut e = Eval { ctx, sim, charge: 0.0, tally: Tally::default() };
    let n = input.panes.len();
    let idle = vec![Sched::STAY; n];
    let mut start: Vec<Sched> = start.filter(|s| s.len() == n).map(<[Sched]>::to_vec).unwrap_or_else(|| idle.clone());
    for (p, s) in start.iter_mut().enumerate() {
        if !g.in_time(p, s) {
            *s = Sched::STAY;
        }
    }
    let (mut best, mut mode, active) = solve(&mut e, &start);
    if !e.fits(&best) {
        (best, mode) = fit(&mut e, best, mode);
    }
    let charge = e.charge;
    e.charge = 0.0;
    prune(&mut e, &mut best);
    e.mark(Stage::Prune);
    let idle_cost = e.cost(&idle);
    let c = e.cost(&best);
    let gains = idle_cost.money - c.money >= MIN_GAIN || (c.money <= idle_cost.money + NOISE && idle_cost.eff - c.eff >= MIN_GAIN_EFF);
    if !gains {
        best = idle;
    }
    super::finish::plan(&mut e, &best, mode, active, charge, idle_cost.totals, &[])
}

/// The best schedules from `start` at the charge in force.
pub(super) fn solve(e: &mut Eval, start: &[Sched]) -> (Vec<Sched>, Mode, usize) {
    let mut cur = start.to_vec();
    let mut cost = e.cost(&cur);
    let active = active(e, &cur);
    let movable: Vec<usize> = (0..cur.len()).filter(|&p| e.grid().movable(e.input(), p)).collect();
    e.mark(Stage::Base);
    if let Some(found) = exact::run(e, &cur, &movable) {
        let c = e.cost(&found);
        e.mark(Stage::Exact);
        if better(&c, &cost) {
            cur = found;
        }
        return (cur, Mode::Exact, active.len());
    }
    if let Some(found) = exact::run(e, &cur, &active) {
        let c = e.cost(&found);
        e.mark(Stage::Exact);
        if better(&c, &cost) {
            cur = found;
        }
        return (cur, Mode::Active, active.len());
    }
    for _ in 0..ROUNDS {
        let order = by_stay_cost(e, &cur, &active);
        e.mark(Stage::Base);
        let mut kept = sweep(e, &mut cur, &mut cost, &order);
        e.mark(Stage::Try);
        if !kept {
            let dear: Vec<usize> = order.iter().copied().take(8).collect();
            kept = moves::pairs_and_chains(e, &mut cur, &mut cost, &dear);
            e.mark(Stage::Moves);
        }
        if !kept {
            break;
        }
    }
    let mut dear: Vec<usize> = by_stay_cost(e, &cur, &active).into_iter().take(POLISH).collect();
    dear.sort_unstable();
    polish(e, &mut cur, &mut cost, &dear);
    e.mark(Stage::Try);
    (cur, Mode::Search, active.len())
}

/// Each of the dearest active panes' every allowed schedule tried once
/// against the plan found, in input order (at most `POLISH` panes, as
/// many as the fleet has today): the sweep tries only the proposals
/// ranked first at linear prices, which miss a switch whose worth lies in
/// what it leaves the panes it parts from.
fn polish(e: &mut Eval, cur: &mut [Sched], cost: &mut Cost, active: &[usize]) {
    for &p in active {
        let all: Vec<Sched> = candidates(e.input(), e.grid(), p).into_iter().filter(|s| allowed(e.input(), e.grid(), &e.ctx.deadline, p, s)).collect();
        for s in std::iter::once(Sched::STAY).chain(all) {
            if s != cur[p] {
                e.try_pane(cur, cost, p, s);
            }
        }
    }
}

/// One sweep over `order`: each pane's best proposals that beat staying
/// at linear prices, each kept only if the simulated cost falls.
fn sweep(e: &mut Eval, cur: &mut [Sched], cost: &mut Cost, order: &[usize]) -> bool {
    let mut kept = false;
    // proposals are priced off the last run, which may be a trial
    // that differs from `cur` in one pane: close enough to rank by
    // the screen is linear only: adding the charge to it either rules out
    // proposals the simulated cost would keep or lets in ones it keeps
    // greedily on the way to a dearer plan
    for &p in order {
        let here = score(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, &cur[p]);
        let mut props = proposals(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, KEEP);
        if cur[p] != Sched::STAY {
            props.push((score(e.input(), e.grid(), &e.sim, &e.ctx.deadline, p, &Sched::STAY), Sched::STAY));
        }
        for (sc, s) in props {
            if sc < here - SCREEN {
                kept |= e.try_pane(cur, cost, p, s);
            }
        }
    }
    kept
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
    evaluate_on(input, &Grid::new(input), scheds, &[])
}

/// As `evaluate`, on the grid `g` of `input` already worked out; `kept`
/// as `finish::plan` takes it.
pub(super) fn evaluate_on(input: &Input, g: &Grid, scheds: &[Sched], kept: &[(u64, u8, f64, f64)]) -> Plan {
    let ctx = Ctx::new(input, g);
    let sim = Sim::new(&ctx);
    let mut e = Eval { ctx, sim, charge: 0.0, tally: Tally::default() };
    let idle = e.cost(&vec![Sched::STAY; input.panes.len()]).totals;
    super::finish::plan(&mut e, scheds, Mode::Search, 0, 0.0, idle, kept)
}

