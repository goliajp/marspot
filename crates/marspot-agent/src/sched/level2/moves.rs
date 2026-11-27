//! Moves one pane cannot make alone, and dropping switches to fit.
//!
//! A pair swap: two panes on different accounts trade at the later of
//! their first allowed moments, each target judged with the other pane
//! gone; tried only where one of the two cannot go alone because the
//! other's account would not last it.  A two-step chain: pane `p` goes
//! to an account `j` that would not last it, and a pane on `j` goes to
//! its best other target at its first allowed moment.  Each is
//! kept only if the simulated cost of the whole plan falls.

use super::grid::{Grid, lasts_high};
use super::propose::{allowed, proposals};
use super::search::{Eval, better};
use super::{Input, LASTS_H, Sched};

const CHAIN_TARGETS: usize = 3;
const CHAIN_MOVERS: usize = 4;

pub(super) fn pairs_and_chains(e: &mut Eval, cur: &mut [Sched], cost: &mut super::search::Cost, dear: &[usize]) -> bool {
    let mut kept = false;
    for (x, &p) in dear.iter().enumerate() {
        for &q in &dear[x + 1..] {
            if let Some(s) = swap(e, cur, p, q) {
                kept |= keep_if_better(e, cur, cost, &s);
            }
        }
    }
    if kept {
        return true;
    }
    for &p in dear {
        for s in chains(e, cur, p) {
            kept |= keep_if_better(e, cur, cost, &s);
        }
    }
    kept
}

fn keep_if_better(e: &mut Eval, cur: &mut [Sched], cost: &mut super::search::Cost, s: &[Sched]) -> bool {
    let c = e.cost(s);
    if better(&c, cost) {
        *cost = c;
        cur.copy_from_slice(s);
        return true;
    }
    false
}

/// The account pane `p` sits on with `cur`'s schedules at cut 0.
fn now_on(g: &Grid, p: usize) -> Option<usize> {
    g.on[p]
}

fn swap(e: &Eval, cur: &[Sched], p: usize, q: usize) -> Option<Vec<Sched>> {
    let (input, g) = (e.input(), e.grid());
    let (i, j) = (now_on(g, p)?, now_on(g, q)?);
    if i == j || cur[p].n > 0 || cur[q].n > 0 || !g.movable(input, p) || !g.movable(input, q) || !g.admits(input, p, j) || !g.admits(input, q, i) {
        return None;
    }
    // only what one pane cannot do alone: one of the two is held back by
    // how long the other's account would last it
    if g.target(input, p, j) && g.target(input, q, i) {
        return None;
    }
    let mut on = g.on.clone();
    on.swap(p, q);
    if lasts_high(input, &on, j, None) < LASTS_H || lasts_high(input, &on, i, None) < LASTS_H {
        return None;
    }
    let at = g.first[p].max(g.first[q]);
    let mut s = cur.to_vec();
    s[p] = Sched::one(j, at);
    s[q] = Sched::one(i, at);
    (allowed(input, g, &e.ctx.deadline, p, &s[p]) && allowed(input, g, &e.ctx.deadline, q, &s[q])).then_some(s)
}

fn chains(e: &mut Eval, cur: &[Sched], p: usize) -> Vec<Vec<Sched>> {
    let (input, g): (&Input, &Grid) = (e.input(), e.grid());
    if cur[p].n > 0 || !g.movable(input, p) {
        return Vec::new();
    }
    e.cost(cur);
    let mut out = Vec::new();
    let first = g.first[p];
    // accounts held back from `p` only by how long they would last it
    let targets: Vec<usize> = (0..input.accounts.len()).filter(|&j| g.admits(input, p, j) && !g.target(input, p, j)).take(CHAIN_TARGETS).collect();
    for j in targets {
        let movers = (0..input.panes.len()).filter(|&r| r != p && g.on[r] == Some(j) && cur[r].n == 0 && g.movable(input, r)).take(CHAIN_MOVERS);
        for r in movers.collect::<Vec<_>>() {
            let Some((_, best)) = proposals(input, g, &e.sim, &e.ctx.deadline, r, 2).into_iter().find(|s| s.1.n == 1 && s.1.hops[0].to as usize != j) else { continue };
            let mut s = cur.to_vec();
            s[p] = Sched::one(j, first.max(best.hops[0].at as usize));
            s[r] = best;
            if allowed(input, g, &e.ctx.deadline, p, &s[p]) {
                out.push(s);
            }
        }
    }
    out
}

/// `s` with counted switches dropped, the one whose removal costs least
/// first, until the plan fits.
pub(super) fn trim(e: &mut Eval, mut s: Vec<Sched>) -> Vec<Sched> {
    while !e.fits(&s) {
        let mut best: Option<(super::search::Cost, usize)> = None;
        for p in 0..s.len() {
            if s[p].n == 0 {
                continue;
            }
            let mut t = s.clone();
            t[p] = Sched::STAY;
            let c = e.cost(&t);
            if best.as_ref().is_none_or(|b| better(&c, &b.0)) {
                best = Some((c, p));
            }
        }
        let Some((_, p)) = best else { break };
        s[p] = Sched::STAY;
    }
    s
}
