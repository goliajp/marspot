//! The pane horizon cut into segments, and what each pane may do.
//!
//! Cut points are the six-hour buckets, every renewal inside the
//! horizon, the moment each pane's earliest switch is allowed, and the
//! latest moment a switch off each account still averts the refusal:
//! one reading and one point of reading error at its load before it
//! empties at the high band if nobody moves.  A switch happens at a cut
//! point.  Targets pass the hard filters at
//! the moment they are proposed: open, fresh, taking the pane's class,
//! not barred, not just left, not this pane's account, and lasting the
//! pane six hours at the high band with the panes already there.

use std::cell::OnceCell;

use super::ctx::Ctx;
use super::window::dead_by_exhaustion;
use super::{Account, H2, Input, Pane};
use crate::quota::EPS;

const BUCKET_H: f64 = 6.0;
/// A move onto an account before its renewal is timed as if the pane
/// ran at this share of its level: what is left there lapses whole when
/// the pane runs slower than its level, while an account it empties
/// early costs one refusal and a move on.  Swept from 0.3 to 0.8 over
/// 638 closed-loop weeks the planner was never tuned on, refusals did not
/// move with it beyond noise and money was lowest at 0.3-0.35, about
/// 0.05 points a week under 0.5; 0.35 also had the fewest weeks dearer
/// than today's rule.
pub(super) const DRAIN_PACE: f64 = 0.35;
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
    /// moved off it before the account's latest moment (`late`).
    pub keeper: Vec<bool>,
    /// Each account's cut at `dead`.
    pub late: Vec<Option<usize>>,
    /// Per account, when it empties at the high band if nobody moves,
    /// capped at the horizon: the deadline a switch off it is taken by.
    pub deadline: Vec<f64>,
    /// Per account that empties inside the horizon, the latest moment a
    /// switch off it averts the refusal: `deadline` less one reading and
    /// one point of reading error at its high-band load; the horizon for
    /// the others.
    pub dead: Vec<f64>,
    /// Per account, the high-band load of the panes on it now; per pane,
    /// its own high-band load.
    load: Vec<f64>,
    pane_high: Vec<f64>,
    /// The same at the level.
    level_load: Vec<f64>,
    pane_level: Vec<f64>,
    /// Each pane's targets, worked out when first asked for.
    targets: Vec<OnceCell<Vec<usize>>>,
}

fn index(accounts: &[Account], id: u8) -> Option<usize> {
    accounts.iter().position(|a| a.q.id == id)
}

fn cut_at(cuts: &[f64], h: f64) -> Option<usize> {
    cuts.iter().position(|c| *c >= h - SAME_H).filter(|&k| k + 1 < cuts.len())
}

impl Grid {
    /// The grid, with a cut at the latest moment a switch off each
    /// account that empties inside the horizon averts the refusal.
    pub fn new(input: &Input) -> Grid {
        let base = Grid::build(input, &[]);
        let deadline = Ctx::deadlines(input, &base);
        let dead: Vec<f64> = deadline.iter().enumerate().map(|(j, d)| if *d < H2 { dead_by_exhaustion(*d, base.load[j]) } else { H2 }).collect();
        let late: Vec<Option<f64>> = dead.iter().map(|t| (*t < H2 && *t > 0.0).then_some(*t)).collect();
        let mut g = if late.iter().all(Option::is_none) {
            base
        } else {
            let extra: Vec<f64> = late.iter().flatten().copied().collect();
            let mut g = Grid::build(input, &extra);
            g.late = late.iter().map(|t| t.and_then(|t| cut_at(&g.cuts, t))).collect();
            g
        };
        (g.deadline, g.dead) = (deadline, dead);
        g.keepers_wait();
        g
    }

    /// A keeper stays to use up its account's rest until the latest
    /// moment a switch off it averts the refusal, and from then may leave
    /// like any pane: a refusal is a loss the user sees, and staying on
    /// to be refused buys nothing.  An account that lasts the horizon
    /// keeps its keeper throughout.
    fn keepers_wait(&mut self) {
        for p in 0..self.keeper.len() {
            let Some(j) = self.on[p].filter(|_| self.keeper[p]) else { continue };
            let wait = match self.late[j] {
                Some(c) => c,
                // past the last cut, or no deadline inside the horizon
                None if self.dead[j] > 0.0 => self.segs(),
                // the latest moment has passed: free now
                None => continue,
            };
            self.first[p] = self.first[p].max(wait);
        }
    }

    /// Every switch of pane `p` off an account that empties inside the
    /// horizon is taken by the latest moment it averts the refusal, but
    /// for a first switch at the pane's first allowed moment, which no
    /// schedule can take earlier.
    pub fn in_time(&self, p: usize, s: &super::Sched) -> bool {
        let mut from = self.on[p];
        s.hops().iter().enumerate().all(|(h, hop)| {
            let at = hop.at as usize;
            let ok = (h == 0 && at == self.first[p]) || from.is_none_or(|j| self.cuts[at] <= self.dead_for(p, j) + SAME_H);
            from = Some(hop.to as usize);
            ok
        })
    }

    /// The latest moment pane `p` leaving account `j` averts the refusal.
    /// Off an account the pane joined, the point of reading error is
    /// taken at the account's load with the pane added.  When the account
    /// empties is still the moment it would if nobody moved: that is the
    /// deadline every switch is timed against.
    pub fn dead_for(&self, p: usize, j: usize) -> f64 {
        if self.on[p] == Some(j) || self.deadline[j] >= H2 {
            return self.dead[j];
        }
        dead_by_exhaustion(self.deadline[j], self.load[j] + self.pane_high[p])
    }

    fn build(input: &Input, extra: &[f64]) -> Grid {
        let mut cuts: Vec<f64> = vec![0.0, H2];
        cuts.extend((1..(H2 / BUCKET_H) as usize).map(|k| k as f64 * BUCKET_H));
        for a in input.accounts {
            cuts.extend([a.q.t, a.q.tf].into_iter().filter(|t| *t > 0.0 && *t < H2));
        }
        cuts.extend(input.panes.iter().map(|p| p.earliest_h).filter(|t| *t > 0.0 && *t < H2));
        cuts.extend_from_slice(extra);
        let on: Vec<Option<usize>> = input.panes.iter().map(|p| index(input.accounts, p.on)).collect();
        let load: Vec<f64> = (0..input.accounts.len()).map(|j| high_load(input, &on, j)).collect();
        let pane_high: Vec<f64> = input.panes.iter().map(|p| p.high[0] + p.high[1]).collect();
        let level_load: Vec<f64> = (0..input.accounts.len()).map(|j| input.panes.iter().enumerate().filter(|(k, _)| on[*k] == Some(j)).map(|(_, p)| p.speed[0] + p.speed[1]).sum()).collect();
        let pane_level: Vec<f64> = input.panes.iter().map(|p| p.speed[0] + p.speed[1]).collect();
        // one cut per account that would lapse week at a renewal inside
        // the horizon: where a pane of the fleet's mean level joining it
        // uses the rest up just by then
        let mean = pane_level.iter().sum::<f64>() / pane_level.len().max(1) as f64;
        cuts.extend((0..input.accounts.len()).filter_map(|k| drain_at(input, &level_load, k, mean)).filter(|t| *t > 0.0));
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|b, a| *b - *a < SAME_H);
        let first = input.panes.iter().map(|p| cut_at(&cuts, p.earliest_h.max(0.0)).unwrap_or(cuts.len() - 1)).collect();
        let renews = input.accounts.iter().map(|a| (a.q.t > 0.0).then(|| cut_at(&cuts, a.q.t)).flatten()).collect();
        let renews_f = input.accounts.iter().map(|a| (a.q.tf > 0.0 && a.q.tf < a.q.t - SAME_H).then(|| cut_at(&cuts, a.q.tf)).flatten()).collect();
        let keeper = keepers(input, &on);
        let late = vec![None; input.accounts.len()];
        let deadline = vec![H2; input.accounts.len()];
        let dead = deadline.clone();
        let targets = vec![OnceCell::new(); input.panes.len()];
        Grid { cuts, on, first, renews, renews_f, keeper, late, deadline, dead, load, pane_high, level_load, pane_level, targets }
    }

    /// The earliest cut from which pane `p` joining account `k` uses up
    /// its week just by its renewal inside the horizon at `DRAIN_PACE` of
    /// its level, with the panes there now at theirs: the level, not the
    /// high band, since at the high band a third of what it should use
    /// would lapse.
    pub fn drain_cut(&self, input: &Input, p: usize, k: usize) -> Option<usize> {
        let at = drain_at(input, &self.level_load, k, self.pane_level[p])?;
        cut_at(&self.cuts, at.max(0.0))
    }

    pub fn segs(&self) -> usize {
        self.cuts.len() - 1
    }

    /// Account `j` passes the hard filters for pane `p` now.
    pub fn target(&self, input: &Input, p: usize, j: usize) -> bool {
        self.admits(input, p, j) && lasts_high(input, &self.on, j, Some(p)) >= input.limits.lasts_h
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
        super::guard::admits(input, pane, q)
    }

    /// Account `k` may take pane `p` once it renews: open, fresh, not
    /// barred, not just left.
    pub fn admits_after_renewal(&self, input: &Input, p: usize, k: usize) -> bool {
        let q = &input.accounts[k].q;
        let pane = &input.panes[p];
        q.open && !q.stale && !(q.id < 64 && pane.barred & (1u64 << q.id) != 0) && !pane.left.is_some_and(|(id, h)| id == q.id && h > q.t)
    }

    /// Every target of pane `p`, ascending by index.
    pub fn targets(&self, input: &Input, p: usize) -> &[usize] {
        self.targets[p].get_or_init(|| (0..input.accounts.len()).filter(|&j| self.target(input, p, j)).collect())
    }

    /// The pane may hold a planned switch at all.
    pub fn movable(&self, input: &Input, p: usize) -> bool {
        !input.stale_feed && input.panes[p].switches_left > 0 && self.first[p] < self.segs()
    }
}

/// When a pane using `h` weeks an hour joining account `k` uses up what
/// the panes there leave at its renewal inside the horizon, just by then
/// at `DRAIN_PACE` of that speed.
pub(super) fn drain_at(input: &Input, load: &[f64], k: usize, h: f64) -> Option<f64> {
    let h = h * DRAIN_PACE;
    let q = &input.accounts[k].q;
    let left = q.r - load[k] * q.t;
    (q.open && q.t > 0.0 && q.t < H2 && h > 0.0 && left > EPS).then(|| q.t - left / h)
}

/// Weeks an hour the panes on account `j` use at the high band.
pub(super) fn high_load(input: &Input, on: &[Option<usize>], j: usize) -> f64 {
    input.panes.iter().enumerate().filter(|(k, _)| on[*k] == Some(j)).map(|(_, p)| p.high[0] + p.high[1]).sum()
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
