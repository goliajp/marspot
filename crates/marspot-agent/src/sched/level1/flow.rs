//! Demand served from items, segment by segment.
//!
//! `edf` is the forward sweep: in each segment a class draws from the
//! usable item that expires first, then the next, until its demand of
//! the segment is met or nothing is left; an item emptying inside a
//! segment hands the rest of the segment to the next one.  For one
//! class whose items are usable over whole intervals that is optimal.
//!
//! With two classes it is not: a renewal brings items that start later,
//! and the restricted class drawing the earlier item can leave the
//! plain class short where the later one would have served both.  So
//! the sweep is followed by `augment`, which reroutes along augmenting
//! paths until none is left; it never lowers what a class already has,
//! so augmenting the restricted class first and the plain class after
//! gives the most restricted demand served, and with it the most demand.

use super::items::{Grid, Item};

/// Below this a flow is nothing.
const TINY: f64 = 1e-12;

/// `[restricted, plain]`.
pub(super) const CLASSES: usize = 2;

pub(super) struct Flow {
    segs: usize,
    items: usize,
    /// `draw[(class × segs + seg) × items + item]`.
    draw: Vec<f64>,
    pub left: Vec<f64>,
    /// `unserved[class × segs + seg]`.
    pub unserved: Vec<f64>,
}

/// Whether `class` may draw from an item.
pub(super) type Allow<'a> = &'a dyn Fn(usize, &Item) -> bool;

impl Flow {
    pub(super) fn new(g: &Grid, demand: [f64; CLASSES]) -> Flow {
        let segs = g.segs();
        let unserved = (0..CLASSES).flat_map(|k| (0..segs).map(move |s| (k, s))).map(|(k, s)| demand[k].max(0.0) * g.len(s)).collect();
        Flow { segs, items: g.items.len(), draw: vec![0.0; CLASSES * segs * g.items.len()], left: g.items.iter().map(|i| i.amount).collect(), unserved }
    }

    fn ix(&self, job: usize, item: usize) -> usize {
        job * self.items + item
    }

    pub(super) fn draw(&self, class: usize, seg: usize, item: usize) -> f64 {
        self.draw[self.ix(class * self.segs + seg, item)]
    }

    pub(super) fn served(&self, class: usize) -> f64 {
        (0..self.segs).flat_map(|s| (0..self.items).map(move |i| (s, i))).map(|(s, i)| self.draw(class, s, i)).sum()
    }

    pub(super) fn unserved_of(&self, class: usize, seg: usize) -> f64 {
        self.unserved[class * self.segs + seg]
    }

    /// What `class` drew from `item` over every segment before `seg`.
    pub(super) fn drawn_before(&self, class: usize, item: usize, seg: usize) -> f64 {
        (0..seg).map(|s| self.draw(class, s, item)).sum()
    }

    /// The forward sweep of `class`; `rank` orders items that expire together.
    pub(super) fn edf(&mut self, g: &Grid, class: usize, allow: Allow, rank: &dyn Fn(&Item) -> u8) {
        let mut order: Vec<usize> = (0..self.items).filter(|&i| allow(class, &g.items[i])).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (&g.items[a], &g.items[b]);
            x.to.total_cmp(&y.to).then(rank(x).cmp(&rank(y))).then(x.acct.cmp(&y.acct))
        });
        for s in 0..self.segs {
            let job = class * self.segs + s;
            for &i in &order {
                if self.unserved[job] <= TINY {
                    break;
                }
                if !g.usable(s, i) || self.left[i] <= TINY {
                    continue;
                }
                let x = self.unserved[job].min(self.left[i]);
                self.apply_direct(job, i, x);
            }
        }
    }

    fn apply_direct(&mut self, job: usize, item: usize, x: f64) {
        let ix = self.ix(job, item);
        self.draw[ix] += x;
        self.unserved[job] = zeroed(self.unserved[job] - x);
        self.left[item] = zeroed(self.left[item] - x);
    }

    /// Augments from the unserved demand of `class` until no path is left.
    pub(super) fn augment(&mut self, g: &Grid, class: usize, allow: Allow) {
        while let Some(path) = self.path(g, class, allow) {
            self.push(&path);
        }
    }

    /// The shortest augmenting path from a job of `class`, as
    /// `[job, item, job, item, …, item]` with job and item node numbers.
    fn path(&self, g: &Grid, class: usize, allow: Allow) -> Option<Vec<usize>> {
        let jobs = CLASSES * self.segs;
        let mut parent = vec![usize::MAX; jobs + self.items];
        let mut queue = std::collections::VecDeque::new();
        for s in 0..self.segs {
            let j = class * self.segs + s;
            if self.unserved[j] > TINY {
                parent[j] = j;
                queue.push_back(j);
            }
        }
        while let Some(node) = queue.pop_front() {
            if node < jobs {
                let (k, s) = (node / self.segs, node % self.segs);
                for i in 0..self.items {
                    let n = jobs + i;
                    if parent[n] != usize::MAX || !g.usable(s, i) || !allow(k, &g.items[i]) {
                        continue;
                    }
                    parent[n] = node;
                    if self.left[i] > TINY {
                        return Some(trace(&parent, n));
                    }
                    queue.push_back(n);
                }
            } else {
                let i = node - jobs;
                for j in 0..jobs {
                    if parent[j] == usize::MAX && self.draw[self.ix(j, i)] > TINY {
                        parent[j] = node;
                        queue.push_back(j);
                    }
                }
            }
        }
        None
    }

    fn push(&mut self, path: &[usize]) {
        let jobs = CLASSES * self.segs;
        let item = |n: usize| n - jobs;
        let end = item(*path.last().unwrap_or(&jobs));
        let mut d = self.unserved[path[0]].min(self.left[end]);
        // a step back along a job's draw can give back no more than it drew
        for w in path[1..].windows(2).step_by(2) {
            d = d.min(self.draw[self.ix(w[1], item(w[0]))]);
        }
        for (k, w) in path.windows(2).enumerate() {
            let (job, it) = if k % 2 == 0 { (w[0], item(w[1])) } else { (w[1], item(w[0])) };
            let ix = self.ix(job, it);
            self.draw[ix] = if k % 2 == 0 { self.draw[ix] + d } else { zeroed(self.draw[ix] - d) };
        }
        self.unserved[path[0]] = zeroed(self.unserved[path[0]] - d);
        self.left[end] = zeroed(self.left[end] - d);
    }
}

fn zeroed(x: f64) -> f64 {
    if x <= TINY { 0.0 } else { x }
}

fn trace(parent: &[usize], end: usize) -> Vec<usize> {
    let mut path = vec![end];
    let mut n = end;
    while parent[n] != n {
        n = parent[n];
        path.push(n);
    }
    path.reverse();
    path
}
