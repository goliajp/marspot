//! The pane horizon cut into segments, and what each pane may do.
//!
//! Cut points are the six-hour buckets, every renewal inside the
//! horizon, and the moment each pane's earliest switch is allowed.  A
//! switch happens at a cut point.  Targets pass the hard filters at
//! the moment they are proposed: open, fresh, taking the pane's class,
//! not barred, not just left, not this pane's account, and lasting the
//! pane six hours at the high band with the panes already there.

use super::{Account, H2, Input, LASTS_H, Pane};
use crate::quota::EPS;

const BUCKET_H: f64 = 6.0;
const SAME_H: f64 = 1e-6;

/// Where accounts and panes are, by index.
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    pub cuts: Vec<f64>,
    /// Each pane's account index; `None` when its account is not read.
    pub on: Vec<Option<usize>>,
    /// Each pane's first allowed cut.
    pub first: Vec<usize>,
    /// Each account's renewal cut inside the horizon.
    pub renews: Vec<Option<usize>>,
    /// Each account's restricted renewal cut, when it comes before the week's.
    pub renews_f: Vec<Option<usize>>,
    /// Each pane is a keeper: it uses up its account's rest and is not
    /// moved off it.
    pub keeper: Vec<bool>,
}

fn index(accounts: &[Account], id: u8) -> Option<usize> {
    accounts.iter().position(|a| a.q.id == id)
}

fn cut_at(cuts: &[f64], h: f64) -> Option<usize> {
    cuts.iter().position(|c| *c >= h - SAME_H).filter(|&k| k + 1 < cuts.len())
}

impl Grid {
    pub fn new(input: &Input) -> Grid {
        let mut cuts: Vec<f64> = vec![0.0, H2];
        cuts.extend((1..(H2 / BUCKET_H) as usize).map(|k| k as f64 * BUCKET_H));
        for a in input.accounts {
            cuts.extend([a.q.t, a.q.tf].into_iter().filter(|t| *t > 0.0 && *t < H2));
        }
        cuts.extend(input.panes.iter().map(|p| p.earliest_h).filter(|t| *t > 0.0 && *t < H2));
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|b, a| *b - *a < SAME_H);
        let on: Vec<Option<usize>> = input.panes.iter().map(|p| index(input.accounts, p.on)).collect();
        let first = input.panes.iter().map(|p| cut_at(&cuts, p.earliest_h.max(0.0)).unwrap_or(cuts.len() - 1)).collect();
        let renews = input.accounts.iter().map(|a| (a.q.t > 0.0).then(|| cut_at(&cuts, a.q.t)).flatten()).collect();
        let renews_f = input.accounts.iter().map(|a| (a.q.tf > 0.0 && a.q.tf < a.q.t - SAME_H).then(|| cut_at(&cuts, a.q.tf)).flatten()).collect();
        let keeper = keepers(input, &on);
        Grid { cuts, on, first, renews, renews_f, keeper }
    }

    pub fn segs(&self) -> usize {
        self.cuts.len() - 1
    }

    /// Account `j` passes the hard filters for pane `p` now.
    pub fn target(&self, input: &Input, p: usize, j: usize) -> bool {
        self.admits(input, p, j) && lasts_high(input, &self.on, j, Some(p)) >= LASTS_H
    }

    /// Every filter but how long the target lasts.
    pub fn admits(&self, input: &Input, p: usize, j: usize) -> bool {
        let pane = &input.panes[p];
        let a = &input.accounts[j];
        let q = &a.q;
        if Some(j) == self.on[p] || !q.open || q.stale || q.r <= EPS || (pane.restricted && q.phi() <= EPS) {
            return false;
        }
        if q.id < 64 && pane.barred & (1u64 << q.id) != 0 {
            return false;
        }
        if pane.left.is_some_and(|(id, h)| id == q.id && h > 0.0) {
            return false;
        }
        true
    }

    /// Account `k` may take pane `p` once it renews: open, fresh, not
    /// barred, not just left.
    pub fn admits_after_renewal(&self, input: &Input, p: usize, k: usize) -> bool {
        let q = &input.accounts[k].q;
        let pane = &input.panes[p];
        q.open && !q.stale && !(q.id < 64 && pane.barred & (1u64 << q.id) != 0) && !pane.left.is_some_and(|(id, h)| id == q.id && h > q.t)
    }

    /// Every target of pane `p`, ascending by index.
    pub fn targets(&self, input: &Input, p: usize) -> Vec<usize> {
        (0..input.accounts.len()).filter(|&j| self.target(input, p, j)).collect()
    }

    /// The pane may hold a planned switch at all.
    pub fn movable(&self, input: &Input, p: usize) -> bool {
        !input.stale_feed && !self.keeper[p] && input.panes[p].switches_left > 0 && self.first[p] < self.segs()
    }
}

/// Hours account `j` lasts at the high band with the panes on it, and
/// pane `extra` added.
pub fn lasts_high(input: &Input, on: &[Option<usize>], j: usize, extra: Option<usize>) -> f64 {
    let (mut a, mut b) = (0.0, 0.0);
    for (k, p) in input.panes.iter().enumerate() {
        if on[k] == Some(j) || Some(k) == extra {
            a += p.high[0];
            b += p.high[1];
        }
    }
    let q = &input.accounts[j].q;
    let mut t = if a + b > 0.0 { q.r / (a + b) } else { f64::INFINITY };
    if a > 0.0 {
        t = t.min(q.phi() / a);
    }
    t
}

/// On each account one pane stays to use up its rest: on an account
/// whose week is all restricted window, the pane with the highest share
/// of restricted use (none that uses no restricted model); elsewhere the
/// slowest pane.  Only where the account would otherwise be left with
/// quota: it empties at the high band inside the horizon.
fn keepers(input: &Input, on: &[Option<usize>]) -> Vec<bool> {
    let mut out = vec![false; input.panes.len()];
    for (j, a) in input.accounts.iter().enumerate() {
        if lasts_high(input, on, j, None) >= H2 {
            continue;
        }
        let here = || input.panes.iter().enumerate().filter(|(k, _)| on[*k] == Some(j));
        let all_window = a.q.n() <= EPS;
        let pick = if all_window {
            let share = |p: &Pane| p.speed[0] / (p.speed[0] + p.speed[1]).max(1e-12);
            here().filter(|(_, p)| p.speed[0] > 0.0).max_by(|x, y| share(x.1).total_cmp(&share(y.1)).then(y.1.sid.cmp(&x.1.sid)))
        } else {
            let rate = |p: &Pane| p.speed[0] + p.speed[1];
            here().min_by(|x, y| rate(x.1).total_cmp(&rate(y.1)).then(x.1.sid.cmp(&y.1.sid)))
        };
        if let Some((k, _)) = pick {
            out[k] = true;
        }
    }
    out
}
