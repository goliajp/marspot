//! Stored resets in the class plan, by enumeration.
//!
//! What a reset brings back depends on what was used before it is
//! claimed, so it is no fixed item.  Each reset is claimed at its
//! dominant time in the plan without claims: when the account reaches
//! the limit the reset needs, else at the last segment boundary a
//! reading interval before it ends; what it brings back is read from
//! that plan at that time.  Each reset that serves demand alone is a
//! candidate; the most valuable `MAX_ENUM` are swept in every
//! combination and the combination serving the most restricted demand,
//! then the most demand, then with the fewest claims, then with the
//! fewest that bring back both the week and the window, is kept: a
//! reset of the window alone that serves as much is claimed in place of
//! one that brings back both, which stays for when the week binds.
//! Nothing here is optimal beyond that.

use super::items::Item;
use super::{Claim, Offer, Solved, World, solve};
use crate::quota::EPS;

/// Resets swept in every combination: 16 combinations.
const MAX_ENUM: usize = 4;
/// One reading interval, hours.
const MARGIN_H: f64 = 5.0 / 60.0;
/// No reset is claimed this close to the account's own renewal, hours.
const NEAR_RENEWAL_H: f64 = 6.0;
/// A reset worth less than this is not counted as lost, weeks.
const WASTE_MIN: f64 = 0.10;
const WEEK_H: f64 = 168.0;

pub(super) struct Chosen {
    pub items: Vec<Item>,
    pub claims: Vec<Claim>,
    pub combos: usize,
    pub lost: f64,
}

/// One reset claimed: on which account, when, and what changes.
#[derive(Clone)]
struct Edit {
    offer: usize,
    acct: usize,
    at: f64,
    /// The week is replaced: what was left before lapses at the claim.
    cut_pre: bool,
    /// The renewal moves past the horizon.
    drop_post: bool,
    /// Taken out of the account's `N` (it became usable by every class).
    take_n: f64,
    add: Vec<Item>,
    value: f64,
}

fn apply(items: &[Item], edits: &[&Edit]) -> Vec<Item> {
    let mut out: Vec<Item> = items.to_vec();
    for e in edits {
        out.retain(|i| !(e.drop_post && i.acct == e.acct && !i.pre));
        for i in out.iter_mut().filter(|i| i.acct == e.acct && i.pre) {
            if e.cut_pre {
                i.to = i.to.min(e.at);
            }
            if !i.restricted && e.take_n > 0.0 {
                i.amount = (i.amount - e.take_n).max(0.0);
            }
        }
        out.extend(e.add.iter().copied());
    }
    out.retain(|i| i.amount > EPS && i.to > i.from);
    out
}

/// What account `a` has left before its renewal at boundary `b` of the
/// plan `s`: the week, `Phi` and `N`, and what the restricted class drew.
fn left_at(s: &Solved, a: usize, b: usize) -> [f64; 4] {
    let mut out = [0.0; 4];
    for (i, it) in s.grid.items.iter().enumerate().filter(|(_, it)| it.acct == a && it.pre) {
        let d0 = s.flow.drawn_before(0, i, b);
        let rest = (it.amount - d0 - s.flow.drawn_before(1, i, b)).max(0.0);
        out[0] += rest;
        if it.restricted {
            out[1] += rest;
            out[3] += d0;
        } else {
            out[2] += rest;
        }
    }
    out
}

/// The same at time `at`, drawn evenly inside a segment.
fn left_at_time(s: &Solved, a: usize, at: f64) -> [f64; 4] {
    let cuts = &s.grid.cuts;
    let b = cuts.iter().rposition(|&c| c <= at).unwrap_or(0).min(cuts.len() - 2);
    let x = ((at - cuts[b]) / (cuts[b + 1] - cuts[b])).clamp(0.0, 1.0);
    let (lo, hi) = (left_at(s, a, b), left_at(s, a, b + 1));
    [0, 1, 2, 3].map(|k| lo[k] + (hi[k] - lo[k]) * x)
}

/// When the limit `week` (else the restricted window) of account `a`
/// empties in the plan `s`: inside its segment, at the demand rate of
/// the classes that draw it.
fn empties(w: &World, s: &Solved, a: usize, week: bool) -> Option<f64> {
    let cuts = &s.grid.cuts;
    let k = usize::from(!week);
    let b = (1..cuts.len()).find(|&b| left_at(s, a, b)[k] <= EPS)?;
    let before = left_at(s, a, b - 1)[k];
    let rate = if week { w.demand[0] + w.demand[1] } else { w.demand[0] };
    Some(if rate > EPS { (cuts[b - 1] + before / rate).min(cuts[b]) } else { cuts[b] })
}

/// Offer `k` claimed at its dominant time in the plan `s`, if it may be.
fn edit(w: &World, s: &Solved, k: usize, o: &Offer) -> Option<Edit> {
    let a = w.accounts.iter().position(|q| q.id == o.account)?;
    let (q, u) = (&w.accounts[a], &o.supply);
    let at = match (u.requires_limit, u.ends_h) {
        (false, Some(end)) => end - MARGIN_H,
        _ => empties(w, s, a, u.week)?,
    }
    .max(u.starts_h)
    .max(0.0);
    let late = u.ends_h.is_some_and(|e| at > e) || at >= super::H1 || at > q.t - NEAR_RENEWAL_H;
    let [week, _, n_left, drew] = left_at_time(s, a, at);
    let f_left = (q.f - drew).max(0.0);
    let share = if u.week { week } else { f_left / w.window_full };
    if late || u.usable_below_left.is_some_and(|limit| share >= limit) {
        return None;
    }
    let to = if u.week && u.moves_week { at + WEEK_H } else { q.t.max(0.0) };
    let mut e = Edit { offer: k, acct: a, at, cut_pre: u.week, drop_post: u.week && u.moves_week, take_n: 0.0, add: Vec::new(), value: 0.0 };
    let both = |phi: f64| [Item::new(a, true, true, at, to, phi), Item::new(a, false, true, at, to, 1.0 - phi)];
    if u.week && u.window {
        e.add.extend(both(w.window_full.min(1.0)));
    } else if u.week {
        e.add.extend(both(f_left.min(1.0)));
    } else {
        let gain = (w.window_full - f_left).max(0.0);
        let phi_to = q.t.min(q.tf).max(at);
        let x = if u.gives_back_week { gain } else { gain.min(n_left) };
        e.take_n = if u.gives_back_week { 0.0 } else { x };
        e.add.push(Item::new(a, true, true, at, phi_to, x));
        let cut = u.week_cut.of(1.0 - week);
        if cut > EPS {
            e.add.push(Item::new(a, false, true, at, to, cut));
        }
    }
    Some(e)
}

fn better(a: [f64; 2], b: [f64; 2]) -> bool {
    a[0] > b[0] + EPS || (a[0] > b[0] - EPS && a[0] + a[1] > b[0] + b[1] + EPS)
}

/// The claims of `w`'s offers that the plan keeps, and the items with them.
pub(super) fn choose(w: &World, base: &[Item]) -> Chosen {
    let none = Chosen { items: base.to_vec(), claims: Vec::new(), combos: 1, lost: 0.0 };
    if w.offers.is_empty() {
        return none;
    }
    let s = solve(w, base.to_vec());
    let at_base = s.served();
    let mut cands: Vec<Edit> = w
        .offers
        .iter()
        .enumerate()
        .filter_map(|(k, o)| edit(w, &s, k, o))
        .filter_map(|mut e| {
            let got = solve(w, apply(base, &[&e])).served();
            e.value = got[0] + got[1] - at_base[0] - at_base[1];
            better(got, at_base).then_some(e)
        })
        .collect();
    cands.sort_by(|x, y| y.value.total_cmp(&x.value).then(x.offer.cmp(&y.offer)));
    let swept = cands.len().min(MAX_ENUM);
    let both = |b: usize| {
        let u = &w.offers[cands[b].offer].supply;
        u.week && u.window
    };
    let cost = |mask: usize| (mask.count_ones(), (0..swept).filter(|b| mask & (1 << b) != 0 && both(*b)).count());
    let (mut best, mut best_served, mut best_mask) = (base.to_vec(), at_base, 0usize);
    for mask in 1..(1usize << swept) {
        let picked: Vec<&Edit> = (0..swept).filter(|b| mask & (1 << b) != 0).map(|b| &cands[b]).collect();
        let items = apply(base, &picked);
        let got = solve(w, items.clone()).served();
        if better(got, best_served) || (!better(best_served, got) && cost(mask) < cost(best_mask)) {
            (best, best_served, best_mask) = (items, got, mask);
        }
    }
    let kept = |b: usize| b < swept && best_mask & (1 << b) != 0;
    let waits = solve(w, best.clone()).flow.unserved.iter().any(|u| *u > EPS);
    let lost = cands.iter().enumerate().filter(|(b, e)| !kept(*b) && e.value >= WASTE_MIN && waits).map(|(_, e)| e.value).sum();
    let claims = cands
        .iter()
        .enumerate()
        .filter(|(b, _)| kept(*b))
        .map(|(_, e)| Claim { account: w.accounts[e.acct].id, offer: e.offer, at_h: e.at, value: e.value })
        .collect();
    Chosen { items: best, claims, combos: 1 << swept, lost }
}

#[cfg(test)]
#[path = "offers_tests.rs"]
mod tests;
