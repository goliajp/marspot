//! Refusals inside a run: who is refused at a moment and where each
//! refused pane goes.

use super::super::{FableGuard, STALL_REFUSAL_H};
use super::{Ctx, Sim, Totals};
use crate::quota::EPS;

/// Under protection a refusal of an attended pane, which stalls the
/// person at it mid-turn, is charged this much more, weeks (two points,
/// what the profiles' design priced every refusal at).
const ATTENDED_REFUSAL: f64 = 0.02;

impl Sim {
    /// Marks the accounts that stopped serving at `at`, and gives the
    /// panes they refuse.  A pane is refused only on an account that
    /// fails its class, and a pane placed lands where its class is
    /// served: who is refused is known before the first is placed, and
    /// they go in input order.
    fn due_at(&mut self, c: &Ctx, at: f64) -> Vec<usize> {
        let mut due = std::mem::take(&mut self.due);
        due.clear();
        for j in 0..self.r.len() {
            let (plain, window) = (self.serves(c, j, false), self.phi(j) > EPS);
            if self.empty[j].is_none() && (!plain || (self.load[j][0] > 0.0 && !window)) {
                self.empty[j] = Some(at);
            }
            if !plain {
                due.extend_from_slice(&self.at.panes[j]);
            } else if self.at.restricted[j] > 0 && !window {
                due.extend(self.at.panes[j].iter().copied().filter(|&k| c.input.panes[k].restricted));
            }
        }
        due.sort_unstable();
        due
    }

    /// Every pane its account cannot serve at `at` is refused and placed.
    pub(super) fn refuse(&mut self, c: &Ctx, at: f64, band: usize, t: &mut Totals) {
        let due = self.due_at(c, at);
        // who can take a pane does not change while panes are placed
        let mut open = std::mem::take(&mut self.open_to);
        if !due.is_empty() {
            for (cl, v) in open.iter_mut().enumerate() {
                v.clear();
                v.extend((0..self.r.len()).filter(|&j| c.fresh[j] && self.serves(c, j, cl == 0)));
            }
        }
        let mut moved: Option<(usize, Option<usize>)> = None;
        for &k in &due {
            let p = &c.input.panes[k];
            let Some(j) = self.at.on[k] else { continue };
            t.refusals += 1;
            let attended = p.attended && c.input.limits.fable != FableGuard::Price;
            t.refused += p.rewrite + c.input.limits.refusal_extra + if attended { ATTENDED_REFUSAL } else { 0.0 };
            t.eff += STALL_REFUSAL_H;
            if self.refused.len() < 256 {
                self.refused.push((k, at));
            }
            // the loads as a full sum would give them before this pane is
            // placed: only the two accounts the last pane moved between
            // change, and one that serves nobody is read by no fallback
            // before the next step sums every account
            match moved {
                None => self.loads(c),
                Some((a, b)) => {
                    if self.serves(c, a, false) {
                        self.load_of(c, a);
                    }
                    if let Some(b) = b {
                        self.load_of(c, b);
                    }
                }
            }
            let to = self.fallback(c, k, j, band, &open[usize::from(!p.restricted)]);
            self.place(c, k, to);
            moved = Some((j, to));
        }
        self.due = due;
        self.open_to = open;
    }

    pub(super) fn load_of(&mut self, c: &Ctx, j: usize) {
        self.known[j] = self.key(j);
        let (sep, tot) = self.at.sums(c.input, j);
        self.load[j] = if self.phi(j) > EPS { sep } else { [0.0, tot] };
        for cl in 0..2 {
            if c.young_on[cl] == Some(j) && self.serves(c, j, cl == 0) {
                self.load[j][cl] += c.input.young[cl];
            }
        }
    }

    /// Where pane `k` refused on `from` goes, among `open` that take its
    /// class.  Under protection an unattended pane does not go where an
    /// attended one is: it waits instead, and the quota stays for the
    /// person.
    fn fallback(&self, c: &Ctx, k: usize, from: usize, band: usize, open: &[usize]) -> Option<usize> {
        let p = &c.input.panes[k];
        let keep = !p.attended && c.input.limits.fable != FableGuard::Price;
        let held = |j: usize| keep && self.at.panes[j].iter().any(|&o| c.input.panes[o].attended);
        let s = if band == 0 { p.speed } else { p.high };
        let lasts = |j: usize| {
            let [a, b] = self.load[j];
            let mut h = self.r[j] / (a + b + s[0] + s[1]).max(1e-12);
            if p.restricted {
                h = h.min(self.phi(j) / (a + s[0]).max(1e-12));
            }
            h
        };
        // the longest lasting, the lowest index among equals
        let mut best: Option<(usize, f64)> = None;
        for &j in open.iter().filter(|&&j| j != from && !held(j)) {
            let h = lasts(j);
            if best.is_none_or(|b| h.total_cmp(&b.1).is_gt()) {
                best = Some((j, h));
            }
        }
        best.map(|b| b.0)
    }
}
