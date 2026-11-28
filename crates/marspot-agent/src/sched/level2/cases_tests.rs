//! The simulation cases that need no executor.

use super::grid::{DRAIN_PACE, Grid, drain_at, lasts_high};
use super::standing::{Standing, Verdict};
use super::tests::{acct, input, pane};
use super::trigger::{Now, Triggers};
use super::*;

fn herd() -> (Vec<Account>, Vec<Pane>) {
    // P1 empties in 5 h under seven panes; three targets with room
    let a = vec![acct(1, 0.15, 0.0, 100.0), acct(2, 0.25, 0.0, 110.0), acct(3, 0.25, 0.0, 120.0), acct(4, 0.3, 0.0, 130.0)];
    let p = (0..7).map(|k| pane(100 + k, 1, false, 0.0, 0.003 + 0.0003 * k as f64)).collect();
    (a, p)
}

#[test]
fn sched_level2_a_herd_leaves_spread_over_targets_before_the_account_empties() {
    let (a, p) = herd();
    let i = input(&a, &p);
    let plan = plan(&i, None);
    let g = Grid::new(&i);
    let keeper: Vec<u64> = p.iter().enumerate().filter(|(k, _)| g.keeper[*k]).map(|(_, x)| x.sid).collect();
    assert_eq!(keeper, vec![100], "the slowest stays");
    let movers: Vec<u64> = plan.switches.iter().map(|s| s.sid).collect();
    println!("herd: {} of 6 leave, refused {:?}", movers.len(), plan.refusals);
    assert!(movers.len() >= 5, "{:?}", plan.switches);
    // the keeper leaves, if at all, only at P1's latest moment: its window
    // is no longer than a clean rest's lead before it
    let lead = -crate::detmath::ln(1.0 - super::window::CLEAN_CHANCE) / p[0].clean_per_h;
    let latest = plan.switches.iter().map(|s| s.t_dead).fold(0.0, f64::max);
    assert!(plan.switches.iter().filter(|s| s.sid == 100).all(|s| s.t_dead >= latest - 1e-6 && s.t_dead - s.t_open <= lead + 1e-6), "{:?}", plan.switches);
    // at most one pane besides the keeper is left to the refusal
    assert!(plan.refusals.len() <= 2, "{:?}", plan.refusals);
    // when P1 empties if nobody moves
    let empty = a[0].q.r / p.iter().map(|x| x.speed[1]).sum::<f64>();
    for s in &plan.switches {
        assert!(s.t_dead < empty && s.t_dead >= s.t_open, "{s:?} against {empty}");
    }
    // each target carries its newcomers six hours at the high band
    let mut on = g.on.clone();
    for s in &plan.switches {
        let k = p.iter().position(|x| x.sid == s.sid).unwrap();
        on[k] = a.iter().position(|x| x.q.id == s.to);
    }
    for (j, acc) in a.iter().enumerate().skip(1) {
        assert!(lasts_high(&i, &on, j, None) >= LASTS_H, "P{} is overloaded", acc.q.id);
    }
    // the line fits the log's limit with the timestamp and tag in front
    let today: Vec<(u64, u8)> = (100..107).map(|s| (s, 2)).collect();
    let said = line::Said { trigger: "refusal", id: u64::MAX, prev: u64::MAX, bound: 0.123, fleet: (8, 8), notices: (4, 4), refusals: (12, 13), today: &today };
    let l = plan.line(&i, &said);
    assert!(l.len() <= 400, "{} bytes: {l}", l.len());
    let mut to: Vec<u8> = plan.switches.iter().map(|s| s.to).collect();
    to.sort_unstable();
    to.dedup();
    assert!(to.len() >= 2, "the herd lands on more than one account: {to:?}");
}

#[test]
fn sched_level2_herd_planned_at_the_high_band_two_hours_ahead() {
    // the same herd seen with 2.5 h left at the level: the deadline is
    // at least two hours ahead at the high band of × 1.5 only when the
    // plan is made earlier, so the plan made with 5 h left holds every
    // switch before the high-band exhaustion
    let (a, p) = herd();
    let plan = plan(&input(&a, &p), None);
    let high = plan.empty[0][1].unwrap_or(H2);
    assert!(high >= 2.0, "P1 empties at the high band at {high} h");
    assert!(plan.switches.iter().all(|s| s.t_dead <= high), "{:?}", plan.switches);
}

#[test]
fn sched_level2_week_that_would_lapse_at_a_renewal_is_used_up_before_it() {
    // P1 holds 3 points nobody uses and renews in 10 h; P2 empties at
    // the high band in about 13 h.  A pane moves onto P1 in time to use
    // the 3 points up by the renewal, and its window is not put off
    // toward P2's deadline, which lies past the renewal
    let a = [acct(1, 0.03, 0.0, 10.0), acct(2, 0.12, 0.0, 120.0)];
    let p = [pane(10, 2, false, 0.0, 0.004), pane(11, 2, false, 0.0, 0.002)];
    let i = input(&a, &p);
    let idle = evaluate(&i, &[Sched::STAY; 2]);
    let plan = plan(&i, None);
    let s = plan.switches.iter().find(|s| s.to == 1).unwrap_or_else(|| panic!("no move onto P1: {:?}", plan.switches));
    assert!(s.t_open < 10.0 && s.t_dead <= 10.0 + 1e-9 && s.t_open <= s.t_dead, "{s:?}");
    assert!(plan.totals.lapsed < idle.totals.lapsed - 0.01, "lapsed {} against {} idle", plan.totals.lapsed, idle.totals.lapsed);
    assert!(plan.totals.money < idle.totals.money, "money {} against {} idle", plan.totals.money, idle.totals.money);
}

#[test]
fn sched_level2_a_move_off_an_emptying_account_onto_one_renewing_opens_when_planned() {
    // the herd off P1.  With the targets renewing past the horizon each
    // move opens a clean rest's lead before P1's deadline; with P2
    // renewing in 10 h, after the deadline, a move onto it opens when
    // planned and still closes by the deadline
    let (mut a, p) = herd();
    let lead = -crate::detmath::ln(1.0 - super::window::CLEAN_CHANCE) / p[0].clean_per_h;
    let far = plan(&input(&a, &p), None);
    assert!(!far.switches.is_empty() && far.switches.iter().all(|s| s.t_dead - s.t_open <= lead + 1e-6), "{:?}", far.switches);
    let deadline = far.switches.iter().map(|s| s.t_dead).fold(0.0, f64::max);
    a[1] = acct(2, a[1].q.r, 0.0, 10.0);
    let near = plan(&input(&a, &p), None);
    let onto: Vec<_> = near.switches.iter().filter(|s| s.to == 2).collect();
    assert!(!onto.is_empty() && onto.iter().all(|s| s.t_dead <= deadline + 1e-6), "{:?}", near.switches);
    // planned before the deadline it opens when planned; planned at P1's
    // latest moment (the keeper's) it opens the lead before that
    let (early, late): (Vec<&Switch>, Vec<&Switch>) = onto.iter().copied().partition(|s| s.t_open < deadline - lead - 1e-6);
    assert!(!early.is_empty() && early.iter().all(|s| s.t_dead - s.t_open > lead + 1e-6), "{:?}", near.switches);
    assert!(late.iter().all(|s| s.t_dead - s.t_open <= lead + 1e-6), "{:?}", near.switches);
}

#[test]
fn sched_level2_drain_time_is_found_only_for_week_that_would_lapse_inside_the_horizon() {
    let a = [acct(1, 0.03, 0.0, 10.0), acct(2, 0.05, 0.0, 30.0), acct(3, 0.04, 0.0, 10.0)];
    let p = [pane(10, 3, false, 0.0, 0.004)];
    let i = input(&a, &p);
    let load = [0.0, 0.0, 0.004];
    // P1: three points left, at half of 0.01 an hour used up in the 6 h
    // before its renewal
    let t = drain_at(&i, &load, 0, 0.01).expect("P1 drains");
    assert!((t - (10.0 - 0.03 / (0.01 * DRAIN_PACE))).abs() < 1e-9, "{t}");
    // P2 renews past the horizon; P3's own pane uses its week up by then
    assert_eq!(drain_at(&i, &load, 1, 0.01), None);
    assert_eq!(drain_at(&i, &load, 2, 0.01), None);
    // a pane with no speed never drains anything
    assert_eq!(drain_at(&i, &load, 0, 0.0), None);
    // no panes at all: no drain cut, and the grid still builds
    let none = input(&a, &[]);
    let g = Grid::new(&none);
    assert_eq!(g.cuts, vec![0.0, 6.0, 10.0, 12.0, 18.0, 24.0]);
}

#[test]
fn sched_level2_churn_of_helpers_changes_no_structure() {
    // twenty helpers of three minutes each over an hour count only in
    // fleet demand: passes every 2 s, at most one full search a minute
    let (a, p) = herd();
    let mut st = Standing::default();
    let mut trig = Triggers::default();
    let quants: Vec<_> = a.iter().map(|x| x.q.clone()).collect();
    let counted: Vec<u64> = p.iter().map(|x| x.sid).collect();
    let (mut full, mut changes, mut first_id) = (0, 0, None);
    for k in 0..1800i64 {
        let at = 10_000 + 2 * k;
        let helpers = ((k * 2) % 180 < 120) as u8 as f64 * (1 + (k / 90) % 3) as f64;
        let mut i = input(&a, &p);
        i.young = [0.0, 0.0005 * helpers];
        let high = vec![0.0; a.len()];
        let now = Now { at, accounts: &quants, high: &high, counted: &counted, offers: 0, refused: false, ended: false, outside: &[] };
        if let Some(_why) = trig.due(&now) {
            full += 1;
            let r = pass::full(&mut st, &i, 1, at);
            if let Some(Verdict::Adopted { changed }) = r.verdict {
                if first_id.is_some() && changed > 0 {
                    changes += 1;
                }
                first_id.get_or_insert(st.id);
            }
            trig.searched(&now, &r.plan.empty.iter().map(|e| e[1]).collect::<Vec<_>>(), r.plan.cuts[1]);
        } else {
            pass::retime(&mut st, &i, at);
        }
    }
    assert!(full <= 60, "{full} full searches");
    assert_eq!(changes, 0, "the structure changed");
    assert_eq!(first_id, Some(st.id));
}

#[test]
fn sched_level2_a_band_twice_as_wide_flips_few_decisions() {
    // sensitivity on the band: worlds of the herd kind with the high
    // band at × 1.5 (as planned) against × 3, decisions compared per pane
    let mut rng = 0x5eed_u64;
    let mut next = || {
        rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (rng >> 11) as f64 / (1u64 << 53) as f64
    };
    let (mut flips, mut decisions) = (0, 0);
    for _ in 0..50 {
        let a: Vec<Account> = (1..=4).map(|id| acct(id, 0.05 + 0.5 * next(), 0.0, 30.0 + 100.0 * next())).collect();
        let mut p: Vec<Pane> = (0..6).map(|k| pane(100 + k, 1 + (next() * 4.0) as u8, false, 0.0, 0.002 + 0.006 * next())).collect();
        let base = plan(&input(&a, &p), None);
        for x in &mut p {
            x.high = [x.speed[0] * 3.0, x.speed[1] * 3.0];
        }
        let wide = plan(&input(&a, &p), None);
        for x in &p {
            let t = |pl: &Plan| pl.switches.iter().find(|s| s.sid == x.sid && s.order == 0).map(|s| s.to);
            decisions += 1;
            flips += usize::from(t(&base) != t(&wide));
        }
    }
    println!("band × 2: {flips} of {decisions} first switches differ");
    assert!(flips * 10 <= decisions * 3, "{flips} of {decisions}");
}

