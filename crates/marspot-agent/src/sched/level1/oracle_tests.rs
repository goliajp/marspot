//! Level 1 against the exhaustive fluid optimum: a max-flow over a
//! uniform time grid, written apart from the planner, that first
//! serves as much restricted demand as any schedule can and then as
//! much plain demand on top.  Worlds whose times fall on the grid are
//! solved exactly by it.

use super::flow::Flow;
use super::items::{Grid, items_of};
use super::tests::{KAPPA, Rng, q, random_accounts, world};
use super::*;

/// A residual graph with edge lists.
struct Net {
    head: Vec<Vec<usize>>,
    to: Vec<usize>,
    cap: Vec<f64>,
}

impl Net {
    fn new(n: usize) -> Net {
        Net { head: vec![Vec::new(); n], to: Vec::new(), cap: Vec::new() }
    }

    fn edge(&mut self, a: usize, b: usize, c: f64) {
        for (x, y, c) in [(a, b, c), (b, a, 0.0)] {
            self.head[x].push(self.to.len());
            self.to.push(y);
            self.cap.push(c);
        }
    }

    /// Edmonds–Karp from `s` to `t`; the flow added.
    fn max_flow(&mut self, s: usize, t: usize) -> f64 {
        let mut total = 0.0;
        loop {
            let mut via = vec![usize::MAX; self.head.len()];
            let mut queue = std::collections::VecDeque::from([s]);
            while let Some(x) = queue.pop_front() {
                for &e in &self.head[x] {
                    let y = self.to[e];
                    if y != s && via[y] == usize::MAX && self.cap[e] > 1e-13 {
                        via[y] = e;
                        queue.push_back(y);
                    }
                }
            }
            if via[t] == usize::MAX {
                return total;
            }
            let (mut d, mut y) = (f64::INFINITY, t);
            while y != s {
                d = d.min(self.cap[via[y]]);
                y = self.to[via[y] ^ 1];
            }
            let mut y = t;
            while y != s {
                self.cap[via[y]] -= d;
                self.cap[via[y] ^ 1] += d;
                y = self.to[via[y] ^ 1];
            }
            total += d;
        }
    }
}

/// `(restricted served, plain served)` at the optimum; `closed(account,
/// hour)` closes an account.
fn optimum(accounts: &[Quant], demand: [f64; 2], step: f64, closed: &dyn Fn(usize, f64) -> bool) -> (f64, f64) {
    // supply: (account, restricted, from, to, amount)
    let mut items = Vec::new();
    for (a, x) in accounts.iter().enumerate() {
        let phi = x.f.min(x.r).max(0.0);
        items.push((a, true, 0.0, x.t, phi));
        items.push((a, false, 0.0, x.t, x.r - phi));
        items.push((a, true, x.t, f64::INFINITY, KAPPA));
        items.push((a, false, x.t, f64::INFINITY, 1.0 - KAPPA));
    }
    let steps = (H1 / step) as usize;
    let (src, sink) = (0, 1);
    let job = |k: usize, s: usize| 2 + k * steps + s;
    let item = |i: usize| 2 + 2 * steps + i;
    let mut net = Net::new(2 + 2 * steps + items.len());
    for (i, it) in items.iter().enumerate() {
        net.edge(item(i), sink, it.4);
        for s in 0..steps {
            let at = s as f64 * step;
            if it.2 <= at && at + step <= it.3 && !closed(it.0, at) {
                for k in 0..2 {
                    if k == 1 || it.1 {
                        net.edge(job(k, s), item(i), f64::INFINITY);
                    }
                }
            }
        }
    }
    for s in 0..steps {
        net.edge(src, job(0, s), demand[0] * step);
    }
    let restricted = net.max_flow(src, sink);
    for s in 0..steps {
        net.edge(src, job(1, s), demand[1] * step);
    }
    (restricted, net.max_flow(src, sink))
}

fn check(accounts: &[Quant], demand: [f64; 2], step: f64, closures: &[Closure], what: &str) {
    let closed = |a: usize, at: f64| closures.iter().any(|c| c.account == accounts[a].id && c.from_h <= at && at < c.to_h);
    let (r, n) = optimum(accounts, demand, step, &closed);
    let w = World { closures, ..world(accounts, demand, demand[0] * 2.0) };
    let p = plan(&w);
    let tol = 1e-9;
    assert!((p.served[0] - r).abs() < tol, "{what}: restricted served {} against the optimum's {r}", p.served[0]);
    assert!((p.served[1] - n).abs() < tol, "{what}: plain served {} against the optimum's {n}", p.served[1]);
}

/// The worlds of the acceptance: up to 5 accounts, 8 six-hour steps of
/// renewals inside two days and the rest of the week.
#[test]
fn sched_level1_equals_the_exhaustive_fluid_optimum_without_offers_or_rate_windows() {
    let mut rng = Rng(11);
    for w in 0..200 {
        let n = 1 + (rng.next() * 5.0) as usize;
        let accounts = random_accounts(&mut rng, n, 6.0);
        check(&accounts, [rng.next() * 0.03, rng.next() * 0.06], 6.0, &[], &format!("world {w}"));
    }
    // on whole hours, fewer worlds
    for w in 0..20 {
        let accounts = random_accounts(&mut rng, 4, 1.0);
        check(&accounts, [rng.next() * 0.03, rng.next() * 0.06], 1.0, &[], &format!("hourly world {w}"));
    }
}

/// With rate windows no optimality is promised; the gap is measured and
/// is zero, because the augmenting paths do not need intervals.
#[test]
fn sched_level1_rate_windows_leave_no_gap_to_the_optimum() {
    let mut rng = Rng(13);
    for w in 0..100 {
        let n = 1 + (rng.next() * 5.0) as usize;
        let accounts = random_accounts(&mut rng, n, 6.0);
        let a = (rng.next() * accounts.len() as f64) as usize;
        let from = rng.steps(6.0, 20) - 6.0;
        let closures = [Closure { account: accounts[a].id, from_h: from, to_h: from + rng.steps(6.0, 4) }];
        check(&accounts, [rng.next() * 0.03, rng.next() * 0.06], 6.0, &closures, &format!("world {w}"));
    }
}

/// A renewal brings items that start later; the restricted sweep takes
/// the item that expires first, which the plain class needed before the
/// renewal, and only the augmenting paths give it back.
#[test]
fn sched_level1_the_sweep_alone_is_not_exact_once_an_item_starts_later() {
    let a = [q(1, 0.05, 0.05, 10.0), q(2, 0.0, 0.0, 5.0)];
    let demand = [0.005, 0.005];
    let g = Grid::new(items_of(&a, KAPPA), &[], &[1, 2]);
    let mut f = Flow::new(&g, demand);
    let any = |k: usize, it: &items::Item| k == 1 || it.restricted;
    f.edf(&g, 0, &any, &|_| 0);
    f.edf(&g, 1, &any, &|it| u8::from(it.restricted));
    let swept = f.served(0) + f.served(1);
    let (r, n) = optimum(&a, demand, 1.0, &|_, _| false);
    assert!(r + n - swept > 0.02, "the sweep alone serves {swept}, the optimum {}", r + n);
    let p = plan(&world(&a, demand, 0.005));
    assert!((p.served[0] - r).abs() < 1e-9 && (p.served[1] - n).abs() < 1e-9, "{:?} against {r} {n}", p.served);
}
