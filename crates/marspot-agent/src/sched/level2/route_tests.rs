use super::*;
use crate::sched::level2::tests::{acct, input, pane};
use crate::sched::level2::{Sched, evaluate, plan};

fn helper(class: Class) -> Newcomer {
    let rate = crate::quota::RHO0;
    Newcomer { class, rate, high: rate * 1.5, life_h: 0.25 }
}

fn long(class: Class, rate: f64) -> Newcomer {
    Newcomer { class, rate, high: rate * 1.5, life_h: H2 }
}

#[test]
fn sched_route_plain_capacity_first_then_the_renewal_inside_the_day_then_the_longest() {
    // P1 has no week beyond its Fable window; P2 renews in 10 h; P3 and P4 do not
    // renew inside the day, P4 has more room for the same load
    let mut a = [acct(1, 0.3, 0.3, 30.0), acct(2, 0.2, 0.0, 10.0), acct(3, 0.3, 0.0, 100.0), acct(4, 0.6, 0.0, 120.0)];
    // level 1 leaves the plain class some of P1 beyond the reserve
    a[0].spare = 0.1;
    let p = [pane(10, 3, false, 0.0, 0.004), pane(11, 4, false, 0.0, 0.004)];
    let i = input(&a, &p);
    let r = Routing::of(&i, &plan(&i, None));
    assert_eq!(r.order(Class::Plain), vec![2, 4, 3, 1]);
}

#[test]
fn sched_route_a_fable_helper_goes_to_the_fable_capacity_that_renews_first() {
    // P3's Fable window renews in 9 h with room on it; P5 renews in 4 days
    let a = [acct(5, 0.9, 0.4, 96.0), acct(3, 0.1, 0.06, 9.0), acct(7, 0.5, 0.0, 40.0)];
    let p = [pane(10, 5, true, 0.004, 0.0)];
    let i = input(&a, &p);
    let mut r = Routing::of(&i, &plan(&i, None));
    assert_eq!(r.order(Class::Restricted), vec![3, 5]);
    assert_eq!(r.place(&helper(Class::Restricted), None).map(|x| x.to), Some(3));
}

#[test]
fn sched_route_a_batch_of_ten_helpers_spreads_over_the_entries() {
    // each helper is expected to use 0.67 points an hour for 15 minutes;
    // P2 holds about three of them, P4 the rest
    let a = [acct(2, 0.005, 0.0, 10.0), acct(4, 0.6, 0.0, 120.0)];
    let i = input(&a, &[]);
    let mut r = Routing::of(&i, &plan(&i, None));
    let to: Vec<u8> = (0..10).map(|_| r.place(&helper(Class::Plain), None).expect("an entry").to).collect();
    let on2 = to.iter().filter(|t| **t == 2).count();
    assert!((2..=3).contains(&on2), "{to:?}");
    assert!(to[..on2].iter().all(|t| *t == 2) && to[on2..].iter().all(|t| *t == 4), "the first entry first, then the next: {to:?}");
}

#[test]
fn sched_route_the_plan_s_switches_take_room_on_their_target() {
    // the plan holds pane 10's switch off P1, which empties, onto P2
    let a = [acct(1, 0.02, 0.0, 100.0), acct(2, 0.2, 0.0, 120.0), acct(3, 0.2, 0.0, 110.0)];
    let p = [pane(10, 1, false, 0.0, 0.006), pane(11, 1, false, 0.0, 0.002), pane(12, 1, false, 0.0, 0.004)];
    let i = input(&a, &p);
    let pl = plan(&i, None);
    let moved: Vec<u8> = pl.switches.iter().map(|s| s.to).collect();
    assert!(!moved.is_empty());
    let with = Routing::of(&i, &pl);
    let stay = Routing::of(&i, &evaluate(&i, &[Sched::STAY; 3]));
    for to in moved {
        let room = |r: &Routing| r.rooms.iter().find(|x| x.id == to).map(|x| x.left(Class::Plain)).unwrap();
        assert!(room(&with) < room(&stay) - 0.01, "P{to}: {} against {}", room(&with), room(&stay));
    }
}

#[test]
fn sched_route_a_long_newcomer_skips_an_account_that_does_not_last_it_six_hours() {
    // P2 holds 1 point, renews in 30 h: a short helper fits, a pane at 0.6
    // points an hour does not last six hours there
    let a = [acct(2, 0.01, 0.0, 30.0), acct(4, 0.3, 0.0, 120.0)];
    let i = input(&a, &[]);
    let r = Routing::of(&i, &plan(&i, None));
    assert_eq!(r.clone().place(&helper(Class::Plain), None).map(|x| x.to), Some(4), "P4 lasts longest");
    let mut short = r.clone();
    short.rooms.retain(|x| x.id == 2);
    assert_eq!(short.clone().place(&helper(Class::Plain), None).map(|x| x.to), Some(2));
    assert_eq!(short.place(&long(Class::Plain, 0.006), None), None);
}

#[test]
fn sched_route_closed_and_stale_accounts_are_not_entries() {
    let mut a = [acct(1, 0.5, 0.2, 50.0), acct(2, 0.5, 0.2, 60.0), acct(3, 0.5, 0.2, 70.0)];
    a[0].q.open = false;
    a[1].q.stale = true;
    let i = input(&a, &[]);
    let r = Routing::of(&i, &plan(&i, None));
    assert_eq!(r.order(Class::Plain), vec![3]);
    assert_eq!(r.order(Class::Restricted), vec![3]);
}

#[test]
fn sched_route_an_open_agent_goes_where_the_most_is_left() {
    let a = [acct(1, 0.05, 0.0, 100.0)];
    let i = input(&a, &[]);
    let r = Routing::of(&i, &plan(&i, None));
    let m = helper(Class::Plain);
    assert_eq!(r.clone().with_others(&[("b", 0.16), ("c", 0.02)]).agent_for(&m), Some("b"));
    assert_eq!(r.clone().with_others(&[("b", 0.0)]).agent_for(&m), None);
    assert_eq!(r.with_others(&[]).agent_for(&m), None);
}

#[test]
fn sched_route_another_agent_must_hold_the_newcomer_for_its_lifetime() {
    // the planned agent has nothing left; agent b has 2 points: a
    // quarter-hour helper fits, a pane at 0.6 points an hour that lives
    // the day does not
    let a = [acct(1, 0.0, 0.0, 100.0)];
    let i = input(&a, &[]);
    let r = Routing::of(&i, &plan(&i, None)).with_others(&[("b", 0.02)]);
    assert_eq!(r.agent_for(&helper(Class::Plain)), Some("b"));
    assert_eq!(r.agent_for(&long(Class::Plain, 0.006)), None, "two points do not hold a pane that uses 14.4 over the day");
    let big = r.with_others(&[("b", 0.02), ("c", 0.2)]);
    assert_eq!(big.agent_for(&long(Class::Plain, 0.006)), Some("c"));
}

#[test]
fn sched_route_line_lists_each_class_in_order_with_its_room() {
    let a = [acct(2, 0.2, 0.0, 10.0), acct(4, 0.6, 0.1, 120.0)];
    let i = input(&a, &[]);
    let r = Routing::of(&i, &plan(&i, None)).with_others(&[("b", 0.16)]);
    assert_eq!(r.line(), "fable=P4=10.0 plain=P2=20.0,P4=50.0 others=b=16.0");
}

#[test]
fn sched_route_a_moving_pane_is_not_placed_on_the_account_it_leaves() {
    let a = [acct(2, 0.2, 0.0, 10.0), acct(4, 0.6, 0.0, 120.0)];
    let i = input(&a, &[]);
    let mut r = Routing::of(&i, &plan(&i, None));
    let m = Newcomer { class: Class::Plain, rate: 0.004, high: 0.006, life_h: H2 };
    assert_eq!(r.clone().place(&m, None).map(|x| x.to), Some(2), "P2 renews inside the day");
    let q = r.place(&m, Some(2)).expect("P4 holds it");
    assert_eq!(q.to, 4);
    assert!((q.before - q.after - 0.004 * H2).abs() < 1e-9, "{q:?}");
}

/// 2026-11-26 06:20, P6 spent: P3 has 50 points left, all Fable-usable,
/// P7 43 points of other-model room only, P8 98 with about 47 of it.
/// Under balanced, with Fable short, the five Opus panes the refusal
/// sends out go to P7 and P8 and none to P3; under steady P3 is last.
#[test]
fn sched_route_p3_scenario_sends_no_opus_pane_to_fable_only_quota() {
    let mut a = [acct(6, 0.001, 0.0, 120.0), acct(3, 0.5, 0.5, 80.0), acct(7, 0.43, 0.0, 130.0), acct(8, 0.98, 0.51, 150.0)];
    // level 1 lends the plain class a little of P3's reserve
    a[1].spare = 0.05;
    let p = [pane(428, 3, true, 0.004, 0.0)];
    let mut i = input(&a, &p);
    i.limits = crate::sched::level2::Limits::BALANCED;
    let mut r = Routing::of(&i, &plan(&i, None));
    assert!(!r.order(Class::Plain).contains(&3), "{:?}", r.order(Class::Plain));
    let went: Vec<u8> = (0..5).filter_map(|_| r.place(&long(Class::Plain, 0.003), Some(6)).map(|x| x.to)).collect();
    assert_eq!(went.len(), 5, "{went:?}");
    assert!(went.iter().all(|to| *to == 7 || *to == 8), "{went:?}");
    i.limits = crate::sched::level2::Limits::STEADY;
    let s = Routing::of(&i, &plan(&i, None));
    assert_eq!(s.order(Class::Plain).last(), Some(&3));
}
