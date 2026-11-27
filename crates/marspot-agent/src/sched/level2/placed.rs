//! Where each pane sits during a run, with each account's load kept as
//! panes move, so a step of a run costs the accounts and not the panes.
//! An account's sums are taken afresh in pane order the first time they
//! are read after a pane moved on or off it, so they come out bit for
//! bit as summing every pane at every step would.

use super::{Input, Pane, Sched};

pub(super) struct Placed {
    pub on: Vec<Option<usize>>,
    /// Per account, the two speeds of the panes on it summed, and both
    /// summed pane by pane, at the run's band.
    sep: Vec<[f64; 2]>,
    tot: Vec<f64>,
    /// Per account, and for the panes on none, a pane moved since the sum.
    stale: Vec<bool>,
    lost_stale: bool,
    /// Per account, the panes on it in input order, and how many of
    /// them are restricted.
    pub panes: Vec<Vec<usize>>,
    pub restricted: Vec<u32>,
    /// The panes on no account in input order, both speeds summed.
    off: Vec<usize>,
    lost: f64,
    /// The schedules' switches in segment order: segment, pane, account.
    events: Vec<(u8, usize, u8)>,
    next: usize,
    band: usize,
}

impl Placed {
    pub fn new(accounts: usize) -> Placed {
        Placed { on: Vec::new(), sep: vec![[0.0; 2]; accounts], tot: vec![0.0; accounts], stale: vec![false; accounts], lost_stale: false, panes: vec![Vec::new(); accounts], restricted: vec![0; accounts], off: Vec::new(), lost: 0.0, events: Vec::new(), next: 0, band: 0 }
    }

    fn speeds(&self, p: &Pane) -> [f64; 2] {
        if self.band == 0 { p.speed } else { p.high }
    }

    pub fn reset(&mut self, input: &Input, on: &[Option<usize>], scheds: &[Sched], band: usize) {
        self.band = band;
        self.on.clear();
        self.on.extend_from_slice(on);
        self.sep.iter_mut().for_each(|s| *s = [0.0; 2]);
        self.stale.iter_mut().for_each(|s| *s = false);
        self.tot.iter_mut().for_each(|t| *t = 0.0);
        self.panes.iter_mut().for_each(Vec::clear);
        self.restricted.iter_mut().for_each(|n| *n = 0);
        self.off.clear();
        for (k, p) in input.panes.iter().enumerate() {
            let Some(j) = self.on[k] else {
                self.off.push(k);
                continue;
            };
            let s = self.speeds(p);
            self.sep[j][0] += s[0];
            self.sep[j][1] += s[1];
            self.tot[j] += s[0] + s[1];
            self.panes[j].push(k);
            self.restricted[j] += u32::from(p.restricted);
        }
        self.sum_lost(input);
        self.lost_stale = false;
        self.events.clear();
        for (k, sc) in scheds.iter().enumerate() {
            self.events.extend(sc.hops().iter().map(|h| (h.at, k, h.to)));
        }
        // stable: within a segment, panes in input order
        self.events.sort_by_key(|e| e.0);
        self.next = 0;
    }

    /// The next switch due at segment `seg`: pane, account.
    pub fn due(&mut self, seg: usize) -> Option<(usize, usize)> {
        let &(at, k, to) = self.events.get(self.next)?;
        if at as usize != seg {
            return None;
        }
        self.next += 1;
        Some((k, to as usize))
    }

    /// Pane `k` moves to `to`.
    pub fn place(&mut self, input: &Input, k: usize, to: Option<usize>) {
        let from = self.on[k];
        self.on[k] = to;
        let r = u32::from(input.panes[k].restricted);
        match from {
            Some(j) => {
                drop_from(&mut self.panes[j], k);
                self.restricted[j] -= r;
                self.stale[j] = true;
            }
            None => {
                drop_from(&mut self.off, k);
                self.lost_stale = true;
            }
        }
        let s = self.speeds(&input.panes[k]);
        // a pane after every other one there extends the sum in order
        match to {
            Some(j) => {
                let last = add_to(&mut self.panes[j], k);
                self.restricted[j] += r;
                if last && !self.stale[j] {
                    self.sep[j][0] += s[0];
                    self.sep[j][1] += s[1];
                    self.tot[j] += s[0] + s[1];
                } else {
                    self.stale[j] = true;
                }
            }
            None => {
                if add_to(&mut self.off, k) && !self.lost_stale {
                    self.lost += s[0] + s[1];
                } else {
                    self.lost_stale = true;
                }
            }
        }
    }

    /// Account `j`'s two speeds summed, and both summed pane by pane.
    #[inline]
    pub fn sums(&mut self, input: &Input, j: usize) -> ([f64; 2], f64) {
        if self.stale[j] {
            self.sum(input, j);
            self.stale[j] = false;
        }
        (self.sep[j], self.tot[j])
    }

    /// Both speeds of the panes on no account, summed.
    pub fn lost(&mut self, input: &Input) -> f64 {
        if self.lost_stale {
            self.sum_lost(input);
            self.lost_stale = false;
        }
        self.lost
    }

    fn sum(&mut self, input: &Input, j: usize) {
        let (mut a, mut b, mut t) = (0.0, 0.0, 0.0);
        for &k in &self.panes[j] {
            let s = self.speeds(&input.panes[k]);
            a += s[0];
            b += s[1];
            t += s[0] + s[1];
        }
        (self.sep[j], self.tot[j]) = ([a, b], t);
    }

    fn sum_lost(&mut self, input: &Input) {
        self.lost = self.off.iter().map(|&k| self.speeds(&input.panes[k])).map(|s| s[0] + s[1]).sum();
    }
}

fn drop_from(v: &mut Vec<usize>, k: usize) {
    if let Ok(i) = v.binary_search(&k) {
        v.remove(i);
    }
}

/// Whether `k` went in last.
fn add_to(v: &mut Vec<usize>, k: usize) -> bool {
    match v.binary_search(&k) {
        Ok(_) => false,
        Err(i) => {
            v.insert(i, k);
            i + 1 == v.len()
        }
    }
}
