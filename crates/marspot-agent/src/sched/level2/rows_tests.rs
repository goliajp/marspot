//! The logged decisions of 2026-11-26 18:00Z to 18:40Z, planned again.
//!
//! Each row carries the accounts as read and each account's load; the
//! panes on them are the ones measured over the three hours before
//! 18:40Z (points an hour, restricted and plain): 428 on P3 at (0.21,
//! 0.82), 430 on P3 at (0, 0.33), 482 on P5 at (0.17, 1.26), 488 on P8
//! at (0, 0.72), and on P7 0.18 of plain use from two panes.  The row's
//! own pane runs at the row's rate, the other of 441 and 481 at its
//! rate in its own latest row.  Level 1 plans at the row's fleet rates,
//! with the reserve at 3.39 points an hour and the eight six-hour
//! restricted buckets of 11-24 18:00Z to 11-26 18:00Z (fleet demand by
//! six-hour bucket, points an hour).  No pane has a
//! boundary fact, so none is taken to rest cleanly; 441 has long
//! background work.

use super::tests::KAPPA;
use super::*;
use crate::quota::replay::Inputs;
use crate::quota::waste::switch_cost;
use crate::quota::{Class, Quant};
use crate::sched::level1::{self, World};

const ROWS: &str = include_str!("../../../tests/fixtures/alloc_rows_20261126_18z.tsv");
const A_HI: f64 = 0.0339;
const BUCKETS: [f64; 8] = [0.0339, 0.0075, 0.0129, 0.0033, 0.0019, 0.0094, 0.0107, 0.0100];

pub(super) struct Row {
    pub sid: u64,
    pub inputs: Inputs,
}

pub(super) fn rows() -> Vec<Row> {
    ROWS.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Row { sid: f[1].parse().unwrap(), inputs: Inputs::decode(f[5]).expect("the row decodes") }
        })
        .collect()
}

fn p(sid: u64, on: u8, restricted: bool, a: f64, b: f64, tokens: Option<u64>) -> Pane {
    let class = if restricted { Class::Restricted } else { Class::Plain };
    Pane {
        sid,
        on,
        restricted,
        speed: [a, b],
        high: [a * 2.5, b * 1.5],
        rewrite: switch_cost(class, tokens),
        lost_work: 0.0,
        clean_per_h: 0.0,
        long_background: sid == 441,
        earliest_h: 0.0,
        left: None,
        barred: 0,
        switches_left: 6,
        disturb_left: 4,
    }
}

/// The panes of row `r`; `scale` multiplies the loads on P5, P7 and P8.
pub(super) fn panes(r: &Row, scale: f64) -> Vec<Pane> {
    let mut v = vec![p(428, 3, true, 0.0021, 0.0082, None), p(430, 3, false, 0.0, 0.0033, None)];
    for (sid, rate, tokens) in [(441, 0.00551, Some(257_614)), (481, 0.00284, Some(912_688))] {
        let (rate, tokens) = if r.sid == sid { (r.inputs.mover.rate, r.inputs.mover.tokens) } else { (rate, tokens) };
        v.push(p(sid, 3, false, 0.0, rate, tokens));
    }
    v.push(p(482, 5, true, 0.0017 * scale, 0.0126 * scale, Some(789_497)));
    v.push(p(488, 8, false, 0.0, 0.0072 * scale, None));
    v.push(p(483, 7, false, 0.0, 0.0009 * scale, None));
    v.push(p(487, 7, false, 0.0, 0.0009 * scale, None));
    v
}

/// Level 2's accounts for `accounts` at the fleet rates of row `r`, and `pi`.
pub(super) fn accounts(r: &Row) -> (Vec<Account>, f64) {
    let q: Vec<Quant> = r.inputs.accounts.clone();
    let w = World { accounts: &q, window_full: KAPPA, demand: [r.inputs.b_f, r.inputs.b_n], prudent: A_HI, buckets: &BUCKETS, offers: &[], closures: &[], last_pi: None };
    let l1 = level1::plan(&w);
    let acc = q.iter().enumerate().map(|(j, x)| Account { q: x.clone(), spare: l1.spare[j], price: l1.price[j].unwrap_or([0.0, 0.0]) }).collect();
    (acc, l1.pi)
}

pub(super) fn plan_row(r: &Row, scale: f64) -> Plan {
    let (acc, pi) = accounts(r);
    let panes = panes(r, scale);
    let i = Input { accounts: &acc, panes: &panes, young: [0.0; 2], window_full: KAPPA, pi, fleet_left: FLEET_PER_DAY, notices_left: NOTICES_PER_HOUR, stale_feed: false, limits: crate::sched::level2::Limits::STEADY };
    plan(&i, None)
}

fn to_of(plan: &Plan, sid: u64) -> Vec<u8> {
    plan.switches.iter().filter(|s| s.sid == sid).map(|s| s.to).collect()
}

fn line(plan: &Plan) -> String {
    plan.switches.iter().map(|s| format!("{}:P{}>P{}@{:.1}-{:.1}/{}", s.sid, s.from, s.to, s.t_open, s.t_dead, s.kind.word())).collect::<Vec<_>>().join(" ")
}

/// What the rows must give at the logged loads: 441 leaves P3 for P7,
/// 428 stays as the keeper until P3's latest moment and may leave then
/// like any pane, 482 stays, P8 is no target before P3's latest moment.
/// Where 481 goes is printed, not asserted: P3 empties at the high band
/// within two hours, and of 481 and 430, which run at nearly the same
/// plain speed, the plan moves 430 early, whose rewrite costs half of
/// 481's (481 holds 0.9 M tokens), and 481 stays with the keeper to the
/// latest moment.
fn check(plan: &Plan, what: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let latest = plan.switches.iter().filter(|s| s.from == 3).map(|s| s.t_dead).fold(0.0, f64::max);
    // a switch held to P3's latest moment: its window opens after now and
    // closes at that moment
    let at_latest = |s: &Switch| s.from == 3 && s.t_open > 0.0 && (s.t_dead - latest).abs() < 1e-6;
    if to_of(plan, 441) != vec![7] {
        bad.push(format!("{what}: 441 goes {:?}", to_of(plan, 441)));
    }
    if plan.switches.iter().any(|s| s.sid == 428 && !at_latest(s)) || !to_of(plan, 482).is_empty() {
        bad.push(format!("{what}: 428 leaves before P3's latest moment, or 482 moves"));
    }
    if plan.switches.iter().any(|s| s.to == 8 && !at_latest(s)) {
        bad.push(format!("{what}: P8 is a target before P3's latest moment"));
    }
    bad
}

#[test]
fn sched_level2_rows_of_the_evening_plan_441_off_p3_to_p7() {
    let rows = rows();
    assert_eq!(rows.len(), 9);
    let (mut bad, mut p7) = (Vec::new(), 0);
    for (n, r) in rows.iter().enumerate() {
        let plan = plan_row(r, 1.0);
        println!("row {n} pane {}: mode={} {}", r.sid, plan.mode.word(), line(&plan));
        bad.extend(check(&plan, &format!("row {n}")));
        p7 += usize::from(to_of(&plan, 481) == vec![7]);
    }
    println!("481 to P7 in {p7} of {} rows", rows.len());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The targets' loads scaled by 0, 0.5 and 2: printed with how many
/// plans still send 481 to P7; 428 and 482 stay in all.
#[test]
fn sched_level2_rows_with_the_loads_on_the_targets_scaled() {
    let (mut p7, mut all) = (0, 0);
    for (n, r) in rows().iter().enumerate() {
        for scale in [0.0, 0.5, 2.0] {
            let plan = plan_row(r, scale);
            println!("row {n} × {scale}: {}", line(&plan));
            all += 1;
            p7 += usize::from(to_of(&plan, 481) == vec![7]);
            assert!(to_of(&plan, 428).is_empty() && to_of(&plan, 482).is_empty(), "row {n} × {scale}");
        }
    }
    println!("481 to P7 in {p7} of {all} scaled plans");
}

#[test]
fn sched_level2_rows_print_what_441_weighs() {
    let r = &rows()[3];
    let (acc, pi) = accounts(r);
    let panes = panes(r, 1.0);
    let i = Input { accounts: &acc, panes: &panes, young: [0.0; 2], window_full: KAPPA, pi, fleet_left: FLEET_PER_DAY, notices_left: NOTICES_PER_HOUR, stale_feed: false, limits: crate::sched::level2::Limits::STEADY };
    let plan = plan(&i, None);
    println!("row 3: pi {pi:.3}; {}", acc.iter().map(|a| format!("P{} r={:.3} phi={:.3} spare={:.3} price={:.2}/{:.2}", a.q.id, a.q.r, a.q.phi(), a.spare, a.price[0], a.price[1])).collect::<Vec<_>>().join("; "));
    let k = panes.iter().position(|p| p.sid == 441).unwrap();
    for to in [5u8, 7, 8] {
        let j = acc.iter().position(|a| a.q.id == to).unwrap();
        let mut s = plan.scheds.clone();
        s[k] = Sched::one(j, 0);
        let t = evaluate(&i, &s).totals;
        println!("  441 to P{to}: money {:.5} (lapsed {:.5} reserve {:.5} rewrites {:.5} refused {:.5} left {:.5}) eff {:.2} refusals {}", t.money, t.lapsed, t.reserve_taken, t.rewrites, t.refused, t.left_value, t.eff, t.refusals);
    }
    let mut s = plan.scheds.clone();
    s[k] = Sched::STAY;
    let t = evaluate(&i, &s).totals;
    println!("  441 stays: money {:.5} (reserve {:.5} refused {:.5} left {:.5}) eff {:.2} refusals {}", t.money, t.reserve_taken, t.refused, t.left_value, t.eff, t.refusals);
}
