//! Supply as items with a use-by time, and the segments time is cut into.
//!
//! An account is two items until it renews (`Phi`, which every class
//! may use, and `N`, which only the plain class may use) and two more
//! from its renewal on, which outlast the horizon.  When the restricted
//! window renews before the week, `Phi` is cut at the earlier of the
//! two.  A segment is a stretch of time in which every item is either
//! usable throughout or not at all.

use super::{Closure, H1, H2};
use crate::quota::{EPS, Quant};

const WEEK_H: f64 = 168.0;
const BUCKET_H: f64 = 6.0;
/// Two cut points closer than this are one.
const SAME_H: f64 = 1e-9;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Item {
    /// Index of its account in the caller's order.
    pub acct: usize,
    /// Usable by the restricted class (a `Phi` item).
    pub restricted: bool,
    /// The part of a `Phi` item the prudent restricted demand takes.
    pub reserve: bool,
    /// Before the account's renewal: what lapses there if left.
    pub pre: bool,
    pub from: f64,
    pub to: f64,
    pub amount: f64,
}

impl Item {
    pub(super) fn new(acct: usize, restricted: bool, pre: bool, from: f64, to: f64, amount: f64) -> Item {
        Item { acct, restricted, reserve: false, pre, from, to, amount }
    }

    /// Lapses inside the horizon if left.
    pub(super) fn lapses(&self) -> bool {
        self.to <= H1
    }
}

/// The items of `accounts`; `window_full` is what a renewed restricted
/// window holds, in weeks.
pub(super) fn items_of(accounts: &[Quant], window_full: f64) -> Vec<Item> {
    let mut out = Vec::new();
    for (a, q) in accounts.iter().enumerate() {
        let t = q.t.max(0.0);
        let phi = q.phi();
        let n = (q.r.max(0.0) - phi).max(0.0);
        out.push(Item::new(a, true, true, 0.0, t.min(q.tf.max(0.0)), phi));
        out.push(Item::new(a, false, true, 0.0, t, n));
        if t < H1 {
            let phi1 = window_full.min(1.0);
            out.push(Item::new(a, true, false, t, t + WEEK_H, phi1));
            out.push(Item::new(a, false, false, t, t + WEEK_H, 1.0 - phi1));
        }
    }
    out.retain(|i| i.amount > EPS && i.to > i.from);
    out
}

/// Time cut into segments, and which item is usable in which.
pub(super) struct Grid {
    pub cuts: Vec<f64>,
    pub items: Vec<Item>,
    usable: Vec<bool>,
}

impl Grid {
    pub(super) fn new(items: Vec<Item>, closures: &[Closure], ids: &[u8]) -> Grid {
        let mut cuts: Vec<f64> = vec![0.0, H1, H2];
        cuts.extend((1..(H1 / BUCKET_H) as usize).map(|k| k as f64 * BUCKET_H));
        cuts.extend(items.iter().flat_map(|i| [i.from, i.to]));
        cuts.extend(closures.iter().flat_map(|c| [c.from_h, c.to_h]));
        cuts.retain(|c| (0.0..=H1).contains(c));
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|b, a| *b - *a < SAME_H);
        let segs = cuts.len() - 1;
        let mut usable = vec![false; segs * items.len()];
        for s in 0..segs {
            let (lo, hi) = (cuts[s], cuts[s + 1]);
            for (k, it) in items.iter().enumerate() {
                let closed = closures.iter().any(|c| ids.get(it.acct) == Some(&c.account) && c.from_h < hi - SAME_H && c.to_h > lo + SAME_H);
                usable[s * items.len() + k] = it.from <= lo + SAME_H && hi <= it.to + SAME_H && !closed;
            }
        }
        Grid { cuts, items, usable }
    }

    pub(super) fn segs(&self) -> usize {
        self.cuts.len() - 1
    }

    pub(super) fn len(&self, s: usize) -> f64 {
        self.cuts[s + 1] - self.cuts[s]
    }

    pub(super) fn usable(&self, s: usize, i: usize) -> bool {
        self.usable[s * self.items.len() + i]
    }
}
