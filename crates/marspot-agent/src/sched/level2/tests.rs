use super::standing::{Standing, Verdict, plan_id};
use super::*;
use crate::quota::Quant;

pub(crate) const KAPPA: f64 = 0.528;

pub(crate) fn acct(id: u8, r: f64, f: f64, t: f64) -> Account {
    Account { q: Quant { id, r, f, t, tf: t, open: true, stale: false }, spare: (r - f.min(r)).max(0.0), price: [1.0, 1.0] }
}

/// A pane of `restricted` class on `on` at `(a, b)`, high band × 1.5.
pub(crate) fn pane(sid: u64, on: u8, restricted: bool, a: f64, b: f64) -> Pane {
    Pane {
        sid,
        on,
        restricted,
        speed: [a, b],
        high: [a * 1.5, b * 1.5],
        rewrite: 0.001,
        lost_work: 0.0,
        clean_per_h: 4.0,
        long_background: false,
        earliest_h: 0.0,
        left: None,
        barred: 0,
        switches_left: 6,
        disturb_left: 4,
    }
}

pub(crate) fn input<'a>(accounts: &'a [Account], panes: &'a [Pane]) -> Input<'a> {
    Input { accounts, panes, young: [0.0; 2], window_full: KAPPA, pi: 0.5, fleet_left: FLEET_PER_DAY, notices_left: NOTICES_PER_HOUR, stale_feed: false }
}

fn targets(p: &Plan, sid: u64) -> Vec<u8> {
    p.switches.iter().filter(|s| s.sid == sid).map(|s| s.to).collect()
}

#[test]
fn sched_level2_nobody_moves_when_nothing_happens_inside_the_day() {
    let a = [acct(1, 0.8, 0.4, 100.0), acct(2, 0.8, 0.4, 120.0)];
    let p = [pane(10, 1, false, 0.0, 0.005), pane(11, 2, true, 0.004, 0.0)];
    let plan = plan(&input(&a, &p), None);
    assert!(plan.switches.is_empty(), "{:?}", plan.switches);
    assert_eq!(plan.totals.refusals, 0);
}

#[test]
fn sched_level2_a_pane_leaves_an_account_that_runs_out_before_it_does() {
    // P1 holds 2 points against 1.2 an hour; P2 has room
    let a = [acct(1, 0.02, 0.0, 100.0), acct(2, 0.6, 0.0, 120.0)];
    let p = [pane(10, 1, false, 0.0, 0.006), pane(11, 1, false, 0.0, 0.002), pane(12, 1, false, 0.0, 0.004)];
    let plan = plan(&input(&a, &p), None);
    // the slowest stays to use the rest; the others leave before the refusal
    assert_eq!(plan.switches.len(), 2, "{:?}", plan.switches);
    for s in &plan.switches {
        // the window opens as late as still leaves a clean rest at the
        // stopping rule's chance: 4 rests an hour
        let lead = -(1.0 - window::CLEAN_CHANCE).ln() / 4.0;
        assert!(s.sid != 11 && (s.from, s.to) == (1, 2) && (s.t_open - (s.t_dead - lead).max(0.0)).abs() < 1e-9, "{s:?}");
        assert!(s.exempt, "the switch averts a refusal within two hours");
        assert!(s.t_dead > 0.0 && s.t_dead < 2.0 / 1.5, "{s:?}");
    }
    assert_eq!(plan.refusals.len(), 1, "the keeper is refused at the end");
    assert_eq!(plan.refusals[0].0, 11);
    // one pane alone gains 0.2 stall hours by leaving early and the
    // rewrite is paid either way: under the bar, so it stays
    let one = [pane(10, 1, false, 0.0, 0.006), pane(11, 1, false, 0.0, 0.002)];
    assert!(super::plan(&input(&a, &one), None).switches.is_empty());
}

#[test]
fn sched_level2_stale_feed_opens_no_window() {
    let a = [acct(1, 0.02, 0.0, 100.0), acct(2, 0.6, 0.0, 120.0)];
    let p = [pane(10, 1, false, 0.0, 0.006), pane(11, 1, false, 0.0, 0.004)];
    let mut i = input(&a, &p);
    i.stale_feed = true;
    assert!(plan(&i, None).switches.is_empty());
}

#[test]
fn sched_level2_plain_use_of_the_reserve_is_paid_at_pi() {
    // P1: all window, no spare; P2: plain room; the plain pane leaves P1
    let mut a = [acct(1, 0.4, 0.4, 100.0), acct(2, 0.6, 0.0, 120.0)];
    a[0].spare = 0.0;
    let p = [pane(10, 1, false, 0.0, 0.004)];
    let plan = plan(&input(&a, &p), None);
    assert_eq!(targets(&plan, 10), vec![2]);
    assert!(plan.idle.reserve_taken > 0.09 && plan.totals.reserve_taken < 1e-12, "{:?} {:?}", plan.idle, plan.totals);
    assert!(plan.switches[0].gain > 0.04, "{:?}", plan.switches);
}

#[test]
fn sched_level2_a_renewal_inside_the_day_is_a_planned_return() {
    // P1 is spent and renews in 9 h with plain room; P2 is all window
    let mut a = [acct(1, 0.0, 0.0, 9.0), acct(2, 0.5, 0.5, 150.0)];
    a[1].spare = 0.0;
    let p = [pane(10, 2, false, 0.0, 0.004)];
    let plan = plan(&input(&a, &p), None);
    assert_eq!(plan.switches.len(), 1, "{:?}", plan.switches);
    let s = plan.switches[0];
    assert_eq!(s.to, 1);
    assert!((s.t_open - 9.0).abs() < 1e-9, "the return waits for the renewal: {s:?}");
}

#[test]
fn sched_level2_limits_hold_the_pane_and_the_fleet() {
    let mut a = vec![acct(1, 0.0, 0.0, 100.0)];
    a[0].spare = 0.0;
    a[0].q.r = 0.3;
    a[0].q.f = 0.3;
    a.extend((2..=5).map(|id| acct(id, 0.9, 0.0, 120.0)));
    let p: Vec<Pane> = (0..6).map(|k| pane(10 + k, 1, false, 0.0, 0.001)).collect();
    let mut i = input(&a, &p);
    let free = plan(&i, None);
    assert!(free.switches.len() >= 4, "{:?}", free.switches);
    i.fleet_left = 2;
    let tight = plan(&i, None);
    let counted = tight.switches.iter().filter(|s| !s.exempt).count();
    assert!(counted <= 2, "{:?}", tight.switches);
    assert!(tight.charge > 0.0 || counted < free.switches.len());
    // a pane out of switches for the week, or still in its dwell, holds none now
    let mut q = p.clone();
    q[0].switches_left = 0;
    q[1].earliest_h = 3.0;
    let held = plan(&input(&a, &q), None);
    assert!(targets(&held, 10).is_empty());
    assert!(held.switches.iter().filter(|s| s.sid == 11).all(|s| s.t_open >= 3.0 - 1e-9), "{:?}", held.switches);
}

#[test]
fn sched_level2_plan_ids_chain_and_repeat() {
    let s = [(1, 2, 0), (3, 4, 1)];
    assert_eq!(plan_id(7, 9, &s), plan_id(7, 9, &s));
    assert_ne!(plan_id(7, 9, &s), plan_id(8, 9, &s));
    assert_ne!(plan_id(7, 9, &s), plan_id(7, 9, &s[..1]));
}

#[test]
fn sched_level2_a_standing_structure_is_kept_unless_beaten_by_the_bar() {
    let mut a = [acct(1, 0.4, 0.4, 100.0), acct(2, 0.6, 0.0, 120.0), acct(3, 0.6, 0.0, 121.0)];
    a[0].spare = 0.0;
    let p = [pane(10, 1, false, 0.0, 0.004)];
    let i = input(&a, &p);
    let mut st = Standing::default();
    let first = pass::full(&mut st, &i, 1, 1000);
    assert!(matches!(first.verdict, Some(Verdict::Adopted { .. })));
    assert_eq!(st.switches.len(), 1);
    let to = st.switches[0].to;
    let id = st.id;
    // P3 a hair cheaper: the standing target stays
    a[2].price = [1.0, 0.999];
    let i = input(&a, &p);
    let again = pass::full(&mut st, &i, 2, 1060);
    assert_eq!(st.switches[0].to, to);
    assert!(matches!(again.verdict, Some(Verdict::Kept { .. }) | Some(Verdict::Adopted { changed: 0 })), "{:?}", again.verdict);
    assert_eq!(st.id, id, "a kept structure keeps its id");
    // a re-time pass keeps it and moves its window with the clock
    let re = pass::retime(&mut st, &i, 1200).expect("the structure holds");
    assert_eq!(re.plan.switches.len(), 1);
}

#[test]
fn sched_level2_a_committed_switch_whose_gain_is_gone_is_cancelled_and_pinned() {
    let mut a = [acct(1, 0.4, 0.4, 100.0), acct(2, 0.6, 0.0, 120.0)];
    a[0].spare = 0.0;
    let p = [pane(10, 1, false, 0.0, 0.004)];
    let mut st = Standing::default();
    pass::full(&mut st, &input(&a, &p), 1, 1000);
    assert_eq!(st.committed(1000).count(), 1, "a window opening now is committed");
    // P1 gains plain room: nothing is saved by leaving
    a[0].spare = 1.0;
    let r = pass::full(&mut st, &input(&a, &p), 2, 1100);
    assert_eq!(r.ended.cancelled, vec![(10, "gain gone")]);
    assert!(st.pinned_h(10, 1100).is_some_and(|h| (h - 1.0).abs() < 0.01));
}

#[test]
fn sched_level2_a_second_switch_keeps_the_whole_dwell_after_the_first() {
    // a renewal cut 0.4 s before the dwell bound: a window stored to the
    // second reads back at the bound, and must not snap to that cut
    let a = [acct(1, 0.9, 0.3, 100.0), acct(2, 0.9, 0.3, 100.0), acct(3, 0.5, 0.2, DWELL_H - 0.4 / 3600.0)];
    let p = [pane(10, 1, false, 0.0, 0.002)];
    let i = input(&a, &p);
    let g = Grid::new(&i);
    let now = 1_800_000_000;
    let planned = |order: u8, from: u8, to: u8, open_at: i64| standing::Planned { sid: 10, from, to, order, open_at, dead_at: open_at + 3600, gain: 0.01 };
    let mut st = Standing::default();
    st.switches = vec![planned(0, 1, 2, now), planned(1, 2, 3, now + (DWELL_H * 3600.0) as i64)];
    let s = st.schedules(&i, &g, now).expect("the structure holds");
    let (first, second) = (g.cuts[s[0].hops[0].at as usize], g.cuts[s[0].hops[1].at as usize]);
    assert!(second >= first + DWELL_H - 1e-9, "second switch at {second} h, first at {first} h");
}

#[test]
fn sched_level2_leaving_an_account_it_joined_takes_the_reading_error_at_the_load_with_the_pane() {
    let a = [acct(1, 0.9, 0.3, 100.0), acct(2, 0.05, 0.0, 100.0)];
    let p = [pane(10, 1, false, 0.0, 0.002), pane(11, 2, false, 0.0, 0.002)];
    let i = input(&a, &p);
    let g = Grid::new(&i);
    assert!(g.deadline[1] < H2, "account 2 lasts the day");
    let with = grid::high_load(&i, &g.on, 1) + p[0].high[1];
    assert_eq!(g.dead_for(0, 1), window::dead_by_exhaustion(g.deadline[1], with));
    assert!(g.dead_for(0, 1) > g.dead[1]);
    assert_eq!(g.dead_for(1, 1), g.dead[1]);
}
