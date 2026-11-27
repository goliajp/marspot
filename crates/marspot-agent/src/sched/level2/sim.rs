//! A set of schedules run forward over the pane horizon.
//!
//! Week left at a renewal inside the horizon lapses and is paid for
//! whether or not level 1 finds the fleet short there: level 1 can
//! spread demand over the accounts at will, the panes cannot, and
//! putting the quota that lapses first to use is never worse in the
//! fluid plan.  Week the loads at the end of the horizon leave unused
//! at a renewal within the next two days is paid for the same way.
//!
//! Inside a segment every rate is constant; where an account's week or
//! restricted window empties the segment is split there.  A pane on an
//! account that can no longer serve it is refused: it pays its rewrite
//! and a quarter of a stall hour, and the refusal path places it on the
//! account that lasts longest with it added; with none left its demand
//! goes unserved.  A plain pane's restricted use on an account whose
//! window is spent falls to the other models.  Demand below pane level
//! sits on the account with the most room for its class and is not
//! moved.

use super::grid::Grid;
use super::window::kind;
use super::{H2, Input, STALL_REFUSAL_H, Sched};
use crate::quota::EPS;

/// A renewal this soon after the horizon is near enough for the loads
/// at its end to say what lapses there, hours.
const LAPSE_AFTER_H: f64 = 3.0 * H2;

/// What a set of schedules costs, weeks and stall hours.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub lapsed: f64,
    /// Week left at the end that the loads then do not use up before
    /// the account's renewal after the horizon.
    pub lapse_after: f64,
    /// Plain use beyond each account's spare, before `pi`.
    pub reserve_taken: f64,
    pub rewrites: f64,
    pub lost_work: f64,
    /// Rewrites paid at refusals.
    pub refused: f64,
    /// What is left at the end, at level 1's prices.
    pub left_value: f64,
    pub unserved: f64,
    pub money: f64,
    pub eff: f64,
    pub refusals: u16,
    pub switches: u16,
}

/// What every run reads besides the schedules.
pub(super) struct Ctx<'a, 'b> {
    pub input: &'a Input<'b>,
    pub grid: &'a Grid,
    /// Per account, when it empties at the high band if nobody moves,
    /// capped at the horizon: the deadline a switch off it is taken by.
    pub deadline: Vec<f64>,
    young_on: [Option<usize>; 2],
}

impl<'a, 'b> Ctx<'a, 'b> {
    pub fn new(input: &'a Input<'b>, grid: &'a Grid) -> Self {
        let fresh = |j: &usize| input.accounts[*j].q.open && !input.accounts[*j].q.stale;
        let best = |room: &dyn Fn(usize) -> f64| (0..input.accounts.len()).filter(fresh).filter(|&j| room(j) > EPS).max_by(|&a, &b| room(a).total_cmp(&room(b)).then(b.cmp(&a)));
        let young_on = [best(&|j| input.accounts[j].q.phi()), best(&|j| input.accounts[j].q.r)];
        let mut c = Ctx { input, grid, deadline: vec![H2; input.accounts.len()], young_on };
        let mut s = Sim::new(&c);
        let stay = vec![Sched::STAY; input.panes.len()];
        s.run(&c, &stay, 1);
        c.deadline = s.empty.iter().map(|e| e.unwrap_or(H2)).collect();
        c
    }
}

/// Buffers for runs, reused.
pub(super) struct Sim {
    r: Vec<f64>,
    f: Vec<f64>,
    pub plain: Vec<f64>,
    renewed: Vec<bool>,
    load: Vec<[f64; 2]>,
    on: Vec<Option<usize>>,
    /// Per account, when it first stopped serving, hours.
    pub empty: Vec<Option<f64>>,
    /// Refusals: pane index, hours.
    pub refused: Vec<(usize, f64)>,
    /// Per segment and account at the segment's start: week left,
    /// restricted window left, plain use so far.
    pub trace: Vec<[f64; 3]>,
    pub runs: usize,
}

impl Sim {
    pub fn new(c: &Ctx) -> Sim {
        let n = c.input.accounts.len();
        Sim { r: vec![0.0; n], f: vec![0.0; n], plain: vec![0.0; n], renewed: vec![false; n], load: vec![[0.0; 2]; n], on: Vec::new(), empty: vec![None; n], refused: Vec::new(), trace: Vec::new(), runs: 0 }
    }

    fn reset(&mut self, c: &Ctx) {
        for (j, a) in c.input.accounts.iter().enumerate() {
            self.r[j] = a.q.r.max(0.0);
            self.f[j] = a.q.f.max(0.0);
            self.plain[j] = 0.0;
            self.renewed[j] = false;
            self.empty[j] = None;
        }
        self.on.clear();
        self.on.extend_from_slice(&c.grid.on);
        self.refused.clear();
        self.trace.clear();
    }

    fn phi(&self, j: usize) -> f64 {
        self.f[j].min(self.r[j]).max(0.0)
    }

    fn serves(&self, c: &Ctx, j: usize, restricted: bool) -> bool {
        c.input.accounts[j].q.open && self.r[j] > EPS && (!restricted || self.phi(j) > EPS)
    }

    fn loads(&mut self, c: &Ctx, band: usize) {
        self.load.iter_mut().for_each(|l| *l = [0.0; 2]);
        for (k, p) in c.input.panes.iter().enumerate() {
            let Some(j) = self.on[k] else { continue };
            let s = if band == 0 { p.speed } else { p.high };
            if self.phi(j) > EPS {
                self.load[j][0] += s[0];
                self.load[j][1] += s[1];
            } else {
                self.load[j][1] += s[0] + s[1];
            }
        }
        for (cl, j) in c.young_on.iter().enumerate() {
            if let Some(j) = *j
                && self.serves(c, j, cl == 0)
            {
                self.load[j][cl] += c.input.young[cl];
            }
        }
    }

    /// Runs `scheds` at the level (`band` 0) or the high band (1).
    pub fn run(&mut self, c: &Ctx, scheds: &[Sched], band: usize) -> Totals {
        self.runs += 1;
        self.reset(c);
        let mut t = Totals::default();
        let g = c.grid;
        for s in 0..g.segs() {
            self.renew(c, s, &mut t);
            self.hops(c, scheds, s, &mut t);
            for j in 0..self.r.len() {
                let serving = if c.input.accounts[j].q.open { self.r[j] } else { 0.0 };
                self.trace.push([serving, self.phi(j).min(serving), self.plain[j]]);
            }
            let (mut at, end) = (g.cuts[s], g.cuts[s + 1]);
            loop {
                self.refuse(c, at, band, &mut t);
                if at >= end - 1e-12 {
                    break;
                }
                self.loads(c, band);
                let mut dt = end - at;
                for j in 0..self.r.len() {
                    let [a, b] = self.load[j];
                    if a > 0.0 && self.phi(j) > EPS {
                        dt = dt.min(self.phi(j) / a);
                    }
                    if a + b > 0.0 && self.r[j] > EPS {
                        dt = dt.min(self.r[j] / (a + b));
                    }
                }
                self.consume(dt);
                t.unserved += self.unserved(c, band) * dt;
                at += dt;
            }
        }
        self.finish(c, &mut t);
        t
    }

    fn renew(&mut self, c: &Ctx, s: usize, t: &mut Totals) {
        for j in 0..self.r.len() {
            if c.grid.renews[j] == Some(s) && !self.renewed[j] {
                t.lapsed += self.r[j].max(0.0);
                self.r[j] = 1.0;
                self.f[j] = c.input.window_full;
                self.renewed[j] = true;
            } else if c.grid.renews_f[j] == Some(s) && !self.renewed[j] {
                self.f[j] = c.input.window_full;
            }
        }
    }

    fn hops(&mut self, c: &Ctx, scheds: &[Sched], s: usize, t: &mut Totals) {
        for (k, sc) in scheds.iter().enumerate() {
            for h in sc.hops() {
                if h.at as usize != s || self.on[k] == Some(h.to as usize) {
                    continue;
                }
                let p = &c.input.panes[k];
                let dead = self.on[k].map_or(H2, |j| c.deadline[j]);
                let kd = kind(p, c.grid.cuts[s], dead);
                t.rewrites += p.rewrite;
                if kd.notice() {
                    t.lost_work += p.lost_work;
                }
                t.eff += kd.stall();
                t.switches += 1;
                self.on[k] = Some(h.to as usize);
            }
        }
    }

    fn consume(&mut self, dt: f64) {
        for j in 0..self.r.len() {
            let [a, b] = self.load[j];
            self.r[j] -= (a + b) * dt;
            self.f[j] -= a * dt;
            if !self.renewed[j] {
                self.plain[j] += b * dt;
            }
            if self.r[j] < EPS {
                self.r[j] = 0.0;
            }
        }
    }

    fn unserved(&self, c: &Ctx, band: usize) -> f64 {
        let lost: f64 = c.input.panes.iter().zip(&self.on).filter(|(_, o)| o.is_none()).map(|(p, _)| if band == 0 { p.speed[0] + p.speed[1] } else { p.high[0] + p.high[1] }).sum();
        let young: f64 = (0..2).filter(|&cl| c.young_on[cl].is_none_or(|j| !self.serves(c, j, cl == 0))).map(|cl| c.input.young[cl]).sum();
        lost + young
    }

    /// Every pane its account cannot serve at `at` is refused and placed.
    fn refuse(&mut self, c: &Ctx, at: f64, band: usize, t: &mut Totals) {
        for j in 0..self.r.len() {
            if self.empty[j].is_none() && (!self.serves(c, j, false) || (self.load[j][0] > 0.0 && self.phi(j) <= EPS)) {
                self.empty[j] = Some(at);
            }
        }
        for k in 0..c.input.panes.len() {
            let p = &c.input.panes[k];
            let Some(j) = self.on[k] else { continue };
            if self.serves(c, j, p.restricted) {
                continue;
            }
            t.refusals += 1;
            t.refused += p.rewrite;
            t.eff += STALL_REFUSAL_H;
            if self.refused.len() < 256 {
                self.refused.push((k, at));
            }
            self.loads(c, band);
            self.on[k] = self.fallback(c, k, j, band);
        }
    }

    fn fallback(&self, c: &Ctx, k: usize, from: usize, band: usize) -> Option<usize> {
        let p = &c.input.panes[k];
        let s = if band == 0 { p.speed } else { p.high };
        let lasts = |j: usize| {
            let [a, b] = self.load[j];
            let mut h = self.r[j] / (a + b + s[0] + s[1]).max(1e-12);
            if p.restricted {
                h = h.min(self.phi(j) / (a + s[0]).max(1e-12));
            }
            h
        };
        (0..self.r.len())
            .filter(|&j| j != from && !c.input.accounts[j].q.stale && self.serves(c, j, p.restricted))
            .max_by(|&x, &y| lasts(x).total_cmp(&lasts(y)).then(y.cmp(&x)))
    }

    fn finish(&mut self, c: &Ctx, t: &mut Totals) {
        self.loads(c, 0);
        for (j, a) in c.input.accounts.iter().enumerate() {
            if !self.renewed[j] && a.q.t > H2 && a.q.t <= LAPSE_AFTER_H && a.q.open {
                let [x, y] = self.load[j];
                t.lapse_after += (self.r[j] - (x + y) * (a.q.t - H2)).max(0.0);
            }
            let phi = self.phi(j);
            t.left_value += phi * a.price[0] + (self.r[j] - phi).max(0.0) * a.price[1];
            t.reserve_taken += (self.plain[j] - a.spare).max(0.0);
        }
        t.money = t.lapsed + t.lapse_after + c.input.pi * t.reserve_taken + t.rewrites + t.lost_work + t.refused - t.left_value;
    }
}
