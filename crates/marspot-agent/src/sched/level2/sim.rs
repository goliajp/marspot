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

use super::ctx::Ctx;
use super::placed::Placed;
use super::window::kind;
use super::{H2, Sched, Totals};
use crate::quota::EPS;

mod refuse;

/// A renewal this soon after the horizon is near enough for the loads
/// at its end to say what lapses there, hours.
const LAPSE_AFTER_H: f64 = 3.0 * H2;

/// Buffers for runs, reused.
pub(super) struct Sim {
    r: Vec<f64>,
    f: Vec<f64>,
    pub plain: Vec<f64>,
    renewed: Vec<bool>,
    load: Vec<[f64; 2]>,
    at: Placed,
    /// Panes refused at one moment, and the accounts that take each
    /// class then, reused.
    due: Vec<usize>,
    open_to: [Vec<usize>; 2],
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
        Sim { r: vec![0.0; n], f: vec![0.0; n], plain: vec![0.0; n], renewed: vec![false; n], load: vec![[0.0; 2]; n], at: Placed::new(n), due: Vec::new(), open_to: [Vec::new(), Vec::new()], empty: vec![None; n], refused: Vec::new(), trace: Vec::new(), runs: 0 }
    }

    fn reset(&mut self, c: &Ctx, scheds: &[Sched], band: usize) {
        for (j, a) in c.input.accounts.iter().enumerate() {
            self.r[j] = a.q.r.max(0.0);
            self.f[j] = a.q.f.max(0.0);
            self.plain[j] = 0.0;
            self.renewed[j] = false;
            self.empty[j] = None;
        }
        self.at.reset(c.input, &c.grid.on, scheds, band);
        self.refused.clear();
        self.trace.clear();
    }

    fn phi(&self, j: usize) -> f64 {
        self.f[j].min(self.r[j]).max(0.0)
    }

    fn serves(&self, c: &Ctx, j: usize, restricted: bool) -> bool {
        c.open[j] && self.r[j] > EPS && (!restricted || self.phi(j) > EPS)
    }

    /// The loads at the run's band, from the sums kept as panes move.
    fn loads(&mut self, c: &Ctx) {
        for j in 0..self.load.len() {
            self.load_of(c, j);
        }
    }

    /// The loads at `band`, every pane summed.
    fn loads_at(&mut self, c: &Ctx, band: usize) {
        self.load.iter_mut().for_each(|l| *l = [0.0; 2]);
        for (k, p) in c.input.panes.iter().enumerate() {
            let Some(j) = self.at.on[k] else { continue };
            let s = if band == 0 { p.speed } else { p.high };
            if self.phi(j) > EPS {
                self.load[j][0] += s[0];
                self.load[j][1] += s[1];
            } else {
                self.load[j][1] += s[0] + s[1];
            }
        }
        self.young(c);
    }

    fn young(&mut self, c: &Ctx) {
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
        self.reset(c, scheds, band);
        let mut t = Totals::default();
        let g = c.grid;
        for s in 0..g.segs() {
            self.renew(c, s, &mut t);
            self.hops(c, s, &mut t);
            for j in 0..self.r.len() {
                let serving = if c.open[j] { self.r[j] } else { 0.0 };
                self.trace.push([serving, self.phi(j).min(serving), self.plain[j]]);
            }
            let (mut at, end) = (g.cuts[s], g.cuts[s + 1]);
            loop {
                self.refuse(c, at, band, &mut t);
                if at >= end - 1e-12 {
                    break;
                }
                self.loads(c);
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
                t.unserved += self.unserved(c) * dt;
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

    fn hops(&mut self, c: &Ctx, s: usize, t: &mut Totals) {
        while let Some((k, to)) = self.at.due(s) {
            if self.at.on[k] == Some(to) {
                continue;
            }
            let p = &c.input.panes[k];
            let dead = self.at.on[k].map_or(H2, |j| c.deadline[j]);
            let kd = kind(p, c.grid.cuts[s], dead);
            t.rewrites += p.rewrite;
            if kd.notice() {
                t.lost_work += p.lost_work;
            }
            t.eff += kd.stall();
            t.switches += 1;
            self.at.place(c.input, k, Some(to));
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

    fn unserved(&mut self, c: &Ctx) -> f64 {
        let lost = self.at.lost(c.input);
        let young: f64 = (0..2).filter(|&cl| c.young_on[cl].is_none_or(|j| !self.serves(c, j, cl == 0))).map(|cl| c.input.young[cl]).sum();
        lost + young
    }

    fn finish(&mut self, c: &Ctx, t: &mut Totals) {
        self.loads_at(c, 0);
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
