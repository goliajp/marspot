//! What the scheduler costs, held to the budgets in `BUDGETS.md`: each
//! budget is three times the P95 measured on mini.

use marspot_agent::quota::choose::Mover;
use marspot_agent::quota::replay::{Inputs, Kind, Rule};
use marspot_agent::quota::{Class, Load, Quant};
use marspot_agent::sched::price::{Pool, pool};
use marspot_agent::sched::state::State;
use marspot_agent::sched::{POLICY, Terms, alloc, choose, level2, price};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// One scan's decisions: 64 panes, 16 accounts priced, 8 candidates
/// each.  Budget per decision.
const DECIDE_BUDGET: Duration = Duration::from_nanos(8_500);

/// The candidates of one resting pane's account decision built from
/// the objective in force over 16 accounts.  Budget per decision.
const ALLOC_BUDGET: Duration = Duration::from_micros(94);

/// What the scheduler remembers may not grow past this.
const STATE_CAP: usize = 64 * 1024;

fn accounts(n: u8) -> Vec<Quant> {
    (0..n).map(|i| Quant { id: i, r: 0.1 + 0.05 * f64::from(i), f: 0.3, t: 10.0 + 9.0 * f64::from(i), tf: 20.0 + 5.0 * f64::from(i), open: true, stale: false }).collect()
}

fn loads(n: usize) -> Vec<Load> {
    (0..n).map(|i| Load { restricted: 0.002 * (i % 3) as f64, plain: 0.004 + 0.001 * (i % 5) as f64 }).collect()
}

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() * 95 / 100]
}

#[test]
fn sched_decide_perf_gate() {
    let accounts = accounts(16);
    let loads = loads(16);
    let mut state = State::default();
    let mut cands = [(0u8, Terms::default()); 8];
    let mut scans = Vec::new();
    for round in 0..2000 {
        let t0 = Instant::now();
        let pool: Pool = pool(&accounts, &loads);
        for (q, l) in accounts.iter().zip(&loads) {
            black_box(state.price(q.id, price(q, l, pool)));
        }
        for pane in 0..64u64 {
            for (k, c) in cands.iter_mut().enumerate() {
                let x = (pane as f64 * 7.0 + k as f64 * 3.0 + f64::from(round)) % 11.0;
                *c = (k as u8, Terms { money: x * 0.1, efficiency: x * 0.03, fairness: 0.0 });
            }
            let week = state.week_eff(1_800_000_000);
            let ch = choose(black_box(&cands), Some(Terms::default()), week, &POLICY);
            black_box(state.said(pane, u64::from(ch.action.unwrap_or(255)), 1_800_000_000 + i64::from(round)));
        }
        scans.push(t0.elapsed() / 64);
    }
    let p = p95(scans.split_off(100));
    println!("sched decide P95 per decision {p:?}, budget {DECIDE_BUDGET:?}");
    assert!(p < DECIDE_BUDGET, "P95 {p:?} over {DECIDE_BUDGET:?}");
}

#[test]
fn sched_alloc_perf_gate() {
    let accounts = accounts(16);
    let loads = loads(16);
    let kind = Kind::Rest {
        keeper: false,
        since_move_h: None,
        left: Vec::new(),
        band_last_hour: 0,
        demand: 0.1,
        stale_feed: false,
        speed_known: true,
        keeps_restricted: false,
        moves_week: 0,
    };
    let mover = Mover { class: Class::Restricted, rate: 0.004, tokens: Some(300_000), fable: None };
    let i = Inputs { accounts, b_f: 0.02, b_n: 0.08, kappa: Some(0.528), loads, mover, here: 3, kind };
    let mut times = Vec::new();
    for _ in 0..1000 {
        let t0 = Instant::now();
        black_box(alloc::decide(black_box(&i), Rule::waste(0), &|_| true, 0.0, &POLICY));
        times.push(t0.elapsed());
    }
    let p = p95(times.split_off(50));
    println!("sched alloc P95 per decision {p:?}, budget {ALLOC_BUDGET:?}");
    assert!(p < ALLOC_BUDGET, "P95 {p:?} over {ALLOC_BUDGET:?}");
}

#[test]
fn sched_state_bytes() {
    let n = std::mem::size_of::<State>();
    println!("sched state {n} bytes, cap {STATE_CAP}");
    assert!(n <= STATE_CAP, "{n} bytes");
    assert!(n >= 64 * 16, "the pane slots are not all there: {n} bytes");
}

/// One full search of the pane plan at 64 panes × 16 accounts, some of
/// them running out inside the day.  Budget per search.
const PLAN_BUDGET: Duration = Duration::from_millis(824);

/// The standing plan re-timed and re-costed at the same size.
const RETIME_BUDGET: Duration = Duration::from_micros(9487);

fn plan_world(accounts: u8, panes: u64) -> (Vec<level2::Account>, Vec<level2::Pane>) {
    let a = (0..accounts)
        .map(|i| {
            let r = if i % 4 == 0 { 0.03 } else { 0.1 + 0.05 * f64::from(i % 7) };
            let q = Quant { id: i + 1, r, f: 0.3_f64.min(r) * f64::from(i % 2), t: 10.0 + 11.0 * f64::from(i), tf: 10.0 + 11.0 * f64::from(i), open: true, stale: false };
            level2::Account { spare: q.n(), q, price: [0.9, 0.8 + 0.01 * f64::from(i % 5)] }
        })
        .collect();
    let p = (0..panes)
        .map(|k| {
            let restricted = k % 5 == 0;
            let a = if restricted { 0.002 } else { 0.0 };
            let b = 0.001 + 0.0005 * (k % 7) as f64;
            level2::Pane {
                sid: 100 + k,
                on: (k % u64::from(accounts)) as u8 + 1,
                restricted,
                speed: [a, b],
                high: [a * 2.5, b * 1.5],
                rewrite: 0.001,
                lost_work: 0.0,
                clean_per_h: 2.0,
                long_background: k % 9 == 0,
                earliest_h: 0.0,
                left: None,
                barred: 0,
                switches_left: 6,
                disturb_left: 4,
            }
        })
        .collect();
    (a, p)
}

fn plan_input<'a>(a: &'a [level2::Account], p: &'a [level2::Pane]) -> level2::Input<'a> {
    level2::Input { accounts: a, panes: p, young: [0.001, 0.003], window_full: 0.528, pi: 0.5, fleet_left: 8, notices_left: 4, stale_feed: false }
}

#[test]
fn sched_plan_perf_gate() {
    let (a, p) = plan_world(16, 64);
    let i = plan_input(&a, &p);
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..40 {
        let t0 = Instant::now();
        last = Some(black_box(level2::plan(black_box(&i), None)));
        times.push(t0.elapsed());
    }
    let pl = last.expect("a plan");
    let full = p95(times.split_off(5));
    println!("sched plan P95 per full search {full:?} ({} switches, mode {}, {} active, {} simulations), budget {PLAN_BUDGET:?}", pl.switches.len(), pl.mode.word(), pl.active, pl.sims);
    let (a8, p12) = plan_world(8, 12);
    let i8 = plan_input(&a8, &p12);
    let mut small = Vec::new();
    for _ in 0..200 {
        let t0 = Instant::now();
        black_box(level2::plan(black_box(&i8), None));
        small.push(t0.elapsed());
    }
    println!("sched plan P95 at today's size (8 accounts, 12 panes) {:?}", p95(small.split_off(10)));
    assert!(full < PLAN_BUDGET, "P95 {full:?} over {PLAN_BUDGET:?}");
}

#[test]
fn sched_plan_retime_perf_gate() {
    let (a, p) = plan_world(16, 64);
    let i = plan_input(&a, &p);
    let mut st = level2::standing::Standing::default();
    level2::pass::full(&mut st, &i, 1, 1_800_000_000);
    let mut times = Vec::new();
    for k in 0..200 {
        let t0 = Instant::now();
        black_box(level2::pass::retime(&mut st, black_box(&i), 1_800_000_000 + 2 * k));
        times.push(t0.elapsed());
    }
    let p = p95(times.split_off(10));
    println!("sched plan re-time P95 {p:?}, {} standing switches, budget {RETIME_BUDGET:?}", st.switches.len());
    assert!(p < RETIME_BUDGET, "P95 {p:?} over {RETIME_BUDGET:?}");
}

#[test]
fn sched_plan_state_bytes() {
    // the standing plan holds at most 128 switches and 64 pins
    let n = 128 * std::mem::size_of::<level2::standing::Planned>() + 64 * 16 + std::mem::size_of::<level2::standing::Standing>();
    println!("sched standing plan at most {n} bytes, cap {STATE_CAP}");
    assert!(n + std::mem::size_of::<State>() <= STATE_CAP);
}
