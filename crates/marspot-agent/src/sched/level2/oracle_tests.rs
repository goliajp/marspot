//! The plan against the exhaustive search on small worlds, and the
//! fluid bound at every size.
//!
//! The exhaustive search enumerates every pane's every schedule the
//! planner may hold (staying, a switch at its first allowed moment to
//! any target, a return at a renewal, both), jointly over all panes,
//! and keeps the cheapest by the same simulated cost and the same rule
//! against doing nothing.

use super::grid::Grid;
use super::propose::{allowed, candidates};
use super::search::{Cost, better};
use super::sim::{Ctx, Sim};
use super::tests::{KAPPA, acct, input, pane};
use super::*;
use crate::sched::level1::{self, World};

/// The joint schedules enumerated at most.
const JOINT: usize = 40_000;

pub(super) struct Rng(pub u64);

impl Rng {
    pub(super) fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(super) fn below(&mut self, n: usize) -> usize {
        ((self.next() * n as f64) as usize).min(n - 1)
    }
}

pub(super) fn world(rng: &mut Rng, accounts: usize, panes: usize) -> (Vec<Account>, Vec<Pane>) {
    let a: Vec<Account> = (0..accounts)
        .map(|k| {
            let r = 0.01 + 0.3 * rng.next();
            let f = if rng.next() < 0.5 { r * rng.next() } else { 0.0 };
            let t = if rng.next() < 0.4 { 3.0 + 18.0 * rng.next() } else { 30.0 + 130.0 * rng.next() };
            let mut x = acct(k as u8 + 1, r, f, t);
            x.spare = (r - f.min(r)).max(0.0) + 0.1 * rng.next();
            x.price = [0.6 + 0.4 * rng.next(), 0.6 + 0.4 * rng.next()];
            x
        })
        .collect();
    let p = (0..panes)
        .map(|k| {
            let restricted = rng.next() < 0.3;
            let a_ = if restricted { 0.001 + 0.004 * rng.next() } else { 0.0 };
            let mut x = pane(10 + k as u64, 1 + rng.below(accounts) as u8, restricted, a_, 0.001 + 0.006 * rng.next());
            x.rewrite = 0.002 * rng.next();
            x.clean_per_h = 3.0 * rng.next();
            x
        })
        .collect();
    (a, p)
}

/// The cheapest joint schedule, its cost and that of doing nothing;
/// `None` when the joint space is too large.
fn exhaustive(i: &Input) -> Option<(Cost, Cost)> {
    let g = Grid::new(i);
    let ctx = Ctx::new(i, &g);
    let mut sim = Sim::new(&ctx);
    let opts: Vec<Vec<Sched>> = (0..i.panes.len())
        .map(|p| std::iter::once(Sched::STAY).chain(candidates(i, &g, p).into_iter().filter(|s| allowed(i, &g, &ctx.deadline, p, s))).collect())
        .collect();
    if opts.iter().map(Vec::len).product::<usize>() > JOINT {
        return None;
    }
    let mut cost = |s: &[Sched]| {
        let t = sim.run(&ctx, s, 0);
        Cost { money: t.money, eff: t.eff, totals: t }
    };
    let idle = cost(&vec![Sched::STAY; i.panes.len()]);
    let mut best = idle;
    let mut pick = vec![0usize; opts.len()];
    'all: loop {
        let s: Vec<Sched> = pick.iter().enumerate().map(|(p, &k)| opts[p][k]).collect();
        let c = cost(&s);
        if better(&c, &best) {
            best = c;
        }
        for k in (0..pick.len()).rev() {
            pick[k] += 1;
            if pick[k] < opts[k].len() {
                continue 'all;
            }
            pick[k] = 0;
        }
        break;
    }
    let gains = idle.money - best.money >= MIN_GAIN || (best.money <= idle.money + 1e-9 && idle.eff - best.eff >= MIN_GAIN_EFF);
    Some((if gains { best } else { idle }, idle))
}

#[test]
fn sched_level2_equals_the_exhaustive_search_in_exact_mode_and_is_close_otherwise() {
    let mut rng = Rng(29);
    let (mut exact, mut exact_eq, mut search, mut search_close, mut worse_than_idle, mut skipped) = (0, 0, 0, 0, 0, 0);
    let mut gaps = Vec::new();
    while exact + search < 200 {
        let (na, np) = (2 + rng.below(4), 1 + rng.below(4));
        let (a, p) = world(&mut rng, na, np);
        let i = input(&a, &p);
        let Some((ex, idle)) = exhaustive(&i) else {
            skipped += 1;
            continue;
        };
        let plan = plan(&i, None);
        let m = plan.totals.money;
        worse_than_idle += usize::from(m > idle.money + 1e-9);
        let gap = m - ex.money;
        gaps.push(gap);
        if plan.mode == Mode::Exact {
            exact += 1;
            let eq = gap.abs() <= 1e-9 || (gap <= TIE && (plan.totals.eff - ex.eff).abs() <= 1e-9);
            exact_eq += usize::from(eq);
            if !eq {
                println!("  exact miss: gap {gap:.6} eff {:.3} against {:.3}, {} active of {} panes, {} accounts, switches {:?}", plan.totals.eff, ex.eff, plan.active, p.len(), a.len(), plan.switches.iter().map(|s| (s.sid, s.to, s.order)).collect::<Vec<_>>());
            }
        } else {
            search += 1;
            let close = gap <= 0.002_f64.max(0.05 * ex.money.abs());
            search_close += usize::from(close);
            if !close {
                println!("  search miss: gap {gap:.6} money {m:.6} against {:.6} (idle {:.6}), {} active of {} panes, {} accounts, plan {:?} best {:?}", ex.money, idle.money, plan.active, p.len(), a.len(), plan.totals, ex.totals);
            }
        }
    }
    gaps.sort_by(f64::total_cmp);
    println!(
        "oracle: exact {exact_eq}/{exact} equal, search {search_close}/{search} within max(0.2 pt, 5%), worse than idle {worse_than_idle}, skipped {skipped}, gap p50 {:.5} p95 {:.5} max {:.5}",
        gaps[gaps.len() / 2],
        gaps[gaps.len() * 95 / 100],
        gaps[gaps.len() - 1]
    );
    assert_eq!(worse_than_idle, 0);
    assert_eq!(exact_eq, exact, "exact mode missed the optimum");
    assert!(search_close * 100 >= search * 95, "{search_close} of {search}");
}

#[test]
fn sched_level2_gap_to_the_fluid_bound_is_reported_at_every_size() {
    let mut rng = Rng(7);
    let mut rows = Vec::new();
    for _ in 0..200 {
        let (na, np) = (2 + rng.below(15), 2 + rng.below(29));
        let (a, p) = world(&mut rng, na, np);
        let q: Vec<_> = a.iter().map(|x| x.q.clone()).collect();
        let demand = [p.iter().map(|x| x.speed[0]).sum(), p.iter().map(|x| x.speed[1]).sum()];
        let l1 = level1::plan(&World { accounts: &q, window_full: KAPPA, demand, prudent: demand[0], buckets: &[], offers: &[], closures: &[], last_pi: None });
        let mut i = input(&a, &p);
        i.pi = l1.pi;
        let plan = plan(&i, None);
        rows.push((a.len(), p.len(), plan.money_h2(i.pi) - l1.money.total, plan.idle.money - plan.totals.money));
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let gaps: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let gains: Vec<f64> = rows.iter().map(|r| r.3).collect();
    println!("fluid bound: 200 worlds of 2-16 accounts, 2-30 panes; plan money over the day minus the week's bound: mean {:.4} weeks; gain over idle mean {:.4}", mean(&gaps), mean(&gains));
    assert!(gains.iter().all(|g| *g >= -1e-9), "a plan worse than doing nothing");
}
