//! Proposals for one pane: every schedule of at most two switches
//! whose switch times are its first allowed moment, the last moment
//! before its account empties at the high band, and the renewals
//! inside the horizon, scored by linear prices read off the last run.
//! This is the shortest path over (segment, account, switches used)
//! with the switch times restricted to those moments; a switch at any
//! other time pays the same prices later and gains nothing the stopping
//! rule of the window does not.
//!
//! A segment on account `j` costs the pane's two speeds at `j`'s prices
//! then: a restricted point at the price of a point any class can use,
//! a plain point at the price of a plain-only point while the account
//! has some, plus `pi` once the account's plain use has passed its
//! spare.  A pane on an account that stops serving it pays a refusal
//! and the rest of the horizon at a price of one.

use super::grid::Grid;
use super::sim::Sim;
use super::window::kind;
use super::{DWELL_H, H2, Input, STALL_REFUSAL_H, Sched};
use crate::quota::EPS;

/// What a stall hour weighs when proposals are ranked, weeks.
const RANK_STALL: f64 = 0.004;

/// The proposals of pane `p` with their linear scores, cheapest first,
/// without staying; at most `keep`.
pub(super) fn proposals(input: &Input, g: &Grid, sim: &Sim, deadline: &[f64], p: usize, keep: usize) -> Vec<(f64, Sched)> {
    let mut all: Vec<(f64, Sched)> = candidates(input, g, p).into_iter().filter(|s| allowed(input, g, deadline, p, s)).map(|s| (score(input, g, sim, deadline, p, &s), s)).collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0).then(key(input, &a.1).cmp(&key(input, &b.1))));
    all.truncate(keep);
    all
}

/// An order among equals: fewer switches, then the first target's
/// earlier renewal, then its index.
pub(super) fn key(input: &Input, s: &Sched) -> (u8, i64, u8) {
    let first = s.hops().first().map_or((0, 0), |h| ((input.accounts[h.to as usize].q.t * 1e3) as i64, h.to));
    (s.n, first.0, first.1)
}

pub(super) fn candidates(input: &Input, g: &Grid, p: usize) -> Vec<Sched> {
    let mut out = Vec::new();
    if !g.movable(input, p) {
        return out;
    }
    let first = g.first[p];
    let targets = g.targets(input, p);
    for &j in &targets {
        out.push(Sched::one(j, first));
    }
    // the same switch at the last moment it averts the refusal
    if let Some(late) = g.on[p].and_then(|j| g.late[j]).filter(|&c| c > first) {
        for &j in &targets {
            out.push(Sched::one(j, late));
        }
    }
    for (k, r) in g.renews.iter().enumerate() {
        let Some(r) = *r else { continue };
        if r < first || !g.admits_after_renewal(input, p, k) {
            continue;
        }
        if Some(k) != g.on[p] {
            out.push(Sched::one(k, r));
        }
        if input.panes[p].switches_left < 2 {
            continue;
        }
        for &j in &targets {
            if j != k && g.cuts[r] >= g.cuts[first] + DWELL_H - 1e-9 {
                let mut s = Sched::one(j, first);
                s.push(k, r);
                out.push(s);
            }
        }
    }
    out
}

/// The pane's own limits: switches left this week, disturbing actions
/// left today; and every switch off an account that empties is in time
/// to avert the refusal.
pub(super) fn allowed(input: &Input, g: &Grid, deadline: &[f64], p: usize, s: &Sched) -> bool {
    let pane = &input.panes[p];
    if s.n > pane.switches_left || !g.in_time(p, s) {
        return false;
    }
    let disturbing = kinds(input, g, deadline, p, s).filter(|k| k.disturbing()).count();
    disturbing <= pane.disturb_left as usize
}

/// The expected kind of each switch of `s`.
pub(super) fn kinds<'a>(input: &'a Input, g: &'a Grid, deadline: &'a [f64], p: usize, s: &'a Sched) -> impl Iterator<Item = super::Kind> + 'a {
    let mut from = g.on[p];
    s.hops().iter().map(move |h| {
        let dead = from.map_or(H2, |j| deadline[j]);
        from = Some(h.to as usize);
        kind(&input.panes[p], g.cuts[h.at as usize], dead)
    })
}

fn price(input: &Input, sim: &Sim, n: usize, seg: usize, j: usize, class: usize) -> Option<f64> {
    let [r, phi, plain] = sim.trace[seg * n + j];
    let a = &input.accounts[j];
    if r <= EPS || (class == 0 && phi <= EPS) {
        return None;
    }
    if class == 0 {
        return Some(a.price[0]);
    }
    let base = if r - phi > EPS { a.price[1] } else { a.price[0] };
    Some(base + if plain >= a.spare { input.pi } else { 0.0 })
}

/// The linear score of pane `p` following `s`.
pub(super) fn score(input: &Input, g: &Grid, sim: &Sim, deadline: &[f64], p: usize, s: &Sched) -> f64 {
    let pane = &input.panes[p];
    let n = input.accounts.len();
    let mut cost = 0.0;
    for k in kinds(input, g, deadline, p, s) {
        cost += pane.rewrite + if k.notice() { pane.lost_work } else { 0.0 } + RANK_STALL * k.stall();
    }
    let mut on = g.on[p];
    let mut hops = s.hops().iter().peekable();
    for seg in 0..g.segs() {
        while hops.peek().is_some_and(|h| h.at as usize == seg) {
            on = hops.next().map(|h| h.to as usize);
        }
        let len = g.cuts[seg + 1] - g.cuts[seg];
        let Some(j) = on else {
            cost += (pane.speed[0] + pane.speed[1]) * len;
            continue;
        };
        let class = usize::from(!pane.restricted);
        let served = price(input, sim, n, seg, j, class).is_some() && sim.empty[j].is_none_or(|e| e >= g.cuts[seg + 1]);
        if !served {
            cost += pane.rewrite + RANK_STALL * STALL_REFUSAL_H + (pane.speed[0] + pane.speed[1]) * (H2 - g.cuts[seg]);
            break;
        }
        let pa = price(input, sim, n, seg, j, 0).unwrap_or(1.0);
        let pb = price(input, sim, n, seg, j, 1).unwrap_or(1.0);
        cost += (pane.speed[0] * pa + pane.speed[1] * pb) * len;
    }
    cost
}
