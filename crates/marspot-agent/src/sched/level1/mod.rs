//! Level 1 of the planner: the class plan over the week ahead.
//!
//! Supply is a set of items with a use-by time (`items`); demand is two
//! constant flows, restricted and plain.  The restricted class takes
//! `Phi` earliest use-by first, the plain class `N` and what is left of
//! `Phi`, earliest use-by first (`flow::edf`), and augmenting paths
//! finish what the sweep leaves (`flow::augment`): the result serves the
//! most restricted demand and, with it, the most demand.
//!
//! Outputs: per account the reserve the prudent restricted rate takes
//! before the week renews and the spare the plain class may take
//! without touching it; unserved demand per class and segment; the
//! price of the reserve `pi`; per account and class the value of one
//! more point at the end of the pane horizon; the money of the plan
//! (the fluid bound); and the stored resets worth claiming, found by
//! enumerating claim or not per reset (`offers`).  Rate windows close
//! an account for an interval.  Nothing here decides yet.
//!
//! Units: weeks (`0.01` is a point), hours from now.

mod flow;
mod items;
mod line;
mod offers;

use flow::Flow;
use items::{Grid, Item, items_of};

use crate::quota::{EPS, Quant};
use crate::sched::price::held;
use crate::sched::reset::Supply;

/// The class horizon and the pane horizon, hours.
pub const H1: f64 = 168.0;
pub const H2: f64 = 24.0;
/// One point, in weeks.
pub const POINT: f64 = 0.01;
/// Fewer six-hour buckets than this price the reserve at one half.
pub const MIN_BUCKETS: usize = 8;
/// Bisection steps for the rate at which restricted demand goes short.
const BISECT: usize = 7;

/// An account closed by a rate window from `from_h` to `to_h`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Closure {
    pub account: u8,
    pub from_h: f64,
    pub to_h: f64,
}

/// One stored reset of one account.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Offer {
    pub account: u8,
    pub supply: Supply,
}

pub struct World<'a> {
    /// As read, no reset counted in.
    pub accounts: &'a [Quant],
    /// What a renewed restricted window holds, in weeks.
    pub window_full: f64,
    /// `[restricted, plain]` fleet demand, weeks an hour.
    pub demand: [f64; 2],
    /// The prudent restricted rate the reserve is taken at.
    pub prudent: f64,
    /// The fleet's restricted rate in each six-hour bucket observed.
    pub buckets: &'a [f64],
    pub offers: &'a [Offer],
    pub closures: &'a [Closure],
    /// The `pi` last used, for its hysteresis.
    pub last_pi: Option<f64>,
}

/// What the plan costs, in weeks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Money {
    /// Lapsing at a renewal while demand that could use it went unserved.
    pub lapsed: f64,
    /// Taken by the plain class out of the reserve, before `pi`.
    pub reserve_taken: f64,
    /// Resets worth a tenth of a week that go unclaimed while demand waits.
    pub offers_lost: f64,
    pub total: f64,
}

/// A reset the plan claims.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Claim {
    pub account: u8,
    /// Index into `World::offers`.
    pub offer: usize,
    pub at_h: f64,
    /// Demand it serves that would otherwise go unserved, weeks.
    pub value: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Level1 {
    pub cuts: Vec<f64>,
    pub reserve: Vec<f64>,
    pub spare: Vec<f64>,
    /// `[restricted, plain]` per segment.
    pub unserved: [Vec<f64>; 2],
    pub served: [f64; 2],
    pub pi: f64,
    /// Per open account: one more point at the end of the pane horizon,
    /// usable by every class and by the plain class only, is worth this
    /// many points served.
    pub price: Vec<Option<[f64; 2]>>,
    pub money: Money,
    pub claims: Vec<Claim>,
    /// Claim combinations swept.
    pub combos: usize,
}

struct Solved {
    grid: Grid,
    flow: Flow,
}

impl Solved {
    fn served(&self) -> [f64; 2] {
        [self.flow.served(0), self.flow.served(1)]
    }
}

fn any(k: usize, it: &Item) -> bool {
    k == 1 || it.restricted
}

fn off_reserve(k: usize, it: &Item) -> bool {
    if k == 0 { it.restricted } else { !it.reserve }
}

fn rank_restricted(it: &Item) -> u8 {
    u8::from(!it.reserve)
}

fn rank_plain(it: &Item) -> u8 {
    if !it.restricted { 0 } else { 1 + u8::from(it.reserve) }
}

fn ids(w: &World) -> Vec<u8> {
    w.accounts.iter().map(|q| q.id).collect()
}

/// The restricted demand at `rate` alone.
fn restricted_only(g: &Grid, rate: f64) -> Flow {
    let mut f = Flow::new(g, [rate, 0.0]);
    f.edf(g, 0, &any, &rank_restricted);
    f.augment(g, 0, &any);
    f
}

/// `items` with each `Phi` item before its renewal split into what the
/// prudent restricted demand takes of it and the rest.
fn split_reserve(w: &World, items: Vec<Item>) -> Vec<Item> {
    let g = Grid::new(items, w.closures, &ids(w));
    let f = restricted_only(&g, w.prudent);
    let mut out = Vec::with_capacity(g.items.len() + w.accounts.len());
    for (i, it) in g.items.iter().enumerate() {
        if !(it.restricted && it.pre) {
            out.push(*it);
            continue;
        }
        let r = f.drawn_before(0, i, g.segs()).min(it.amount);
        for (amount, reserve) in [(r, true), (it.amount - r, false)] {
            if amount > EPS {
                out.push(Item { amount, reserve, ..*it });
            }
        }
    }
    out
}

fn solve(w: &World, items: Vec<Item>) -> Solved {
    let grid = Grid::new(split_reserve(w, items), w.closures, &ids(w));
    let mut flow = Flow::new(&grid, w.demand);
    flow.edf(&grid, 0, &any, &rank_restricted);
    flow.augment(&grid, 0, &any);
    flow.edf(&grid, 1, &off_reserve, &rank_plain);
    flow.augment(&grid, 1, &off_reserve);
    flow.augment(&grid, 1, &any);
    Solved { grid, flow }
}

/// The class plan of `w`.
pub fn plan(w: &World) -> Level1 {
    let base_items = items_of(w.accounts, w.window_full);
    let chosen = offers::choose(w, &base_items);
    let s = solve(w, chosen.items.clone());
    let segs = s.grid.segs();
    let n = w.accounts.len();
    let (mut reserve, mut r, mut plain) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let mut money = Money { offers_lost: chosen.lost, ..Money::default() };
    for (i, it) in s.grid.items.iter().enumerate() {
        if it.pre {
            r[it.acct] += it.amount;
            if !it.restricted {
                plain[it.acct] += it.amount;
            }
        }
        if it.reserve {
            reserve[it.acct] += it.amount;
            money.reserve_taken += s.flow.drawn_before(1, i, segs);
        }
        let waited = (0..segs).any(|g| s.grid.usable(g, i) && (0..2).any(|k| any(k, it) && s.flow.unserved_of(k, g) > EPS));
        if it.lapses() && waited {
            money.lapsed += s.flow.left[i];
        }
    }
    let spare = (0..n).map(|a| (r[a] - reserve[a]).max(plain[a])).collect();
    let fresh = pi_fresh(w, &s, chosen.items.clone());
    let pi = held(fresh, w.last_pi);
    money.total = money.lapsed + pi * money.reserve_taken + money.offers_lost;
    let price = prices(w, &chosen.items, s.served());
    Level1 {
        cuts: s.grid.cuts.clone(),
        reserve,
        spare,
        unserved: [0, 1].map(|k| (0..segs).map(|g| s.flow.unserved_of(k, g)).collect()),
        served: s.served(),
        pi,
        price,
        money,
        claims: chosen.claims,
        combos: chosen.combos,
    }
}

/// `pi` before its hysteresis: 1 when the restricted demand goes short
/// at its level; else the share of the observed buckets whose rate,
/// held over the horizon, leaves it short; one half with too few buckets.
fn pi_fresh(w: &World, s: &Solved, items: Vec<Item>) -> f64 {
    if (0..s.grid.segs()).any(|g| s.flow.unserved_of(0, g) > EPS) {
        return 1.0;
    }
    if w.buckets.len() < MIN_BUCKETS {
        return 0.5;
    }
    let g = Grid::new(items, w.closures, &ids(w));
    let short = |rate: f64| restricted_only(&g, rate).served(0) < rate * H1 - EPS;
    let (mut lo, mut hi) = (w.demand[0], w.buckets.iter().copied().fold(0.0, f64::max));
    if hi <= lo || !short(hi) {
        return 0.0;
    }
    for _ in 0..BISECT {
        let mid = (lo + hi) / 2.0;
        if short(mid) { hi = mid } else { lo = mid }
    }
    let n = w.buckets.iter().filter(|&&b| b >= hi || (b > lo && short(b))).count();
    n as f64 / w.buckets.len() as f64
}

/// Per open account, what one more point at the end of the pane horizon
/// adds to the demand served, in points per point.
fn prices(w: &World, items: &[Item], base: [f64; 2]) -> Vec<Option<[f64; 2]>> {
    let total = base[0] + base[1];
    w.accounts
        .iter()
        .enumerate()
        .map(|(a, q)| {
            q.open.then(|| {
                [true, false].map(|restricted| {
                    let t = q.t.max(0.0);
                    let to = if t > H2 { t } else { t + 168.0 };
                    let mut more = items.to_vec();
                    more.push(Item::new(a, restricted, t > H2, H2, to, POINT));
                    let s = solve(w, more).served();
                    ((s[0] + s[1] - total) / POINT).max(0.0)
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod grid_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod tests;
