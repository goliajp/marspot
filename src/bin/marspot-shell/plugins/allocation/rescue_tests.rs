//! Rule (b)'s rescue: where Fable would lapse, each window judged
//! against its own renewal, and every limit on it.

use super::choose::Mover;
use super::replay::{Inputs, Kind, Why};
use super::rescue::lapses;
use super::rest_tests::{acct, acct_f, go, resting, with};
use super::{Class, KAPPA_FABLE, Quant};

const FABLE: Mover = Mover { class: Class::Restricted, rate: 0.02 };

#[test]
fn allocation_rescue_sends_fable_where_it_would_lapse() {
    // P2's Fable window lapses in 10 h with a slow Fable pane on it; the
    // fast one on P1 can leave P1, which renews in 100 h; demand exceeds
    // what the two can give an hour
    let a = vec![acct(1, 1.0, KAPPA_FABLE, 100.0), acct(2, 0.3, 0.3, 10.0)];
    let tab = [(Some(1), Class::Restricted, 0.02), (Some(2), Class::Restricted, 0.005), (Some(1), Class::Plain, 0.025)];
    let x = resting(a.clone(), &tab, FABLE, 1, true, 13.0, 0.025, 0.025);
    assert!(super::shortage(&x.accounts, 0.05));
    assert_eq!(go(&x), Some((2, Why::Rescue)));
    // not within twelve hours of its last move
    assert_eq!(go(&resting(a.clone(), &tab, FABLE, 1, true, 11.0, 0.025, 0.025)), None);
    // nor when P1 renews within a day and needs it (demand is raised to
    // keep this a shortage)
    let mut soon = a;
    soon[0].t = 20.0;
    soon[0].tf = 20.0;
    let x = resting(soon, &tab, FABLE, 1, true, 13.0, 0.05, 0.05);
    assert!(super::shortage(&x.accounts, 0.1));
    assert_eq!(go(&x), None);
}

#[test]
fn allocation_lapse_is_judged_at_the_fable_renewal() {
    // the week renews in 2 h with 0.01 left: only that is usable before
    // then, the rest of the window after, and all of it lapses at T_F
    let q = acct_f(4, 0.01, 0.4, 2.0, 20.0);
    assert!((lapses(&q, 0.0) - 0.4).abs() < 1e-12);
    assert!((lapses(&q, 0.01) - (0.4 - 0.01 - 0.18)).abs() < 1e-12);
    // the window renews first: only Φ was usable
    let r = acct_f(5, 0.3, 0.5, 40.0, 10.0);
    assert!((lapses(&r, 0.01) - (0.3 - 0.1)).abs() < 1e-12);
}

/// The fast Fable pane on P1, which renews its Fable in 100 h, with the
/// accounts after P1 as candidates; demand exceeds supply.
fn from_p1(rest: Vec<Quant>) -> Inputs {
    let mut a = vec![acct_f(1, 1.0, KAPPA_FABLE, 30.0, 100.0)];
    a.extend(rest);
    let tab = [(Some(1), Class::Restricted, 0.02), (Some(1), Class::Plain, 0.03)];
    let x = resting(a, &tab, FABLE, 1, true, 13.0, 0.05, 0.03);
    assert!(super::shortage(&x.accounts, 0.08));
    x
}

#[test]
fn allocation_rescue_orders_by_the_fable_renewal() {
    // P2's week renews first but its Fable lasts past P1's; P3's Fable
    // lapses in 20 h
    let x = from_p1(vec![acct_f(2, 0.3, 0.3, 10.0, 150.0), acct_f(3, 0.5, 0.3, 150.0, 20.0)]);
    assert_eq!(go(&x), Some((3, Why::Rescue)));
    // the week renews in 2 h, the Fable window in 20 h: nearly all of
    // that window would lapse, and 0.06 of week lasts the pane 3 h
    assert_eq!(go(&from_p1(vec![acct_f(4, 0.06, 0.4, 2.0, 20.0)])), Some((4, Why::Rescue)));
}

#[test]
fn allocation_rescue_needs_the_week_there_to_last_the_pane() {
    // 0.01 of week lasts the pane half an hour of the two the rescue
    // must be worth: it would be refused there at once
    assert_eq!(go(&from_p1(vec![acct_f(4, 0.01, 0.4, 2.0, 20.0)])), None);
    // the week there also carries the plain pane already on it
    let x = from_p1(vec![acct_f(4, 0.06, 0.4, 2.0, 20.0)]);
    let mut busy = x.clone();
    busy.loads[1].plain = 0.04;
    assert_eq!(go(&busy), None, "0.06 lasts both panes one hour");
}

#[test]
fn allocation_rescue_limits() {
    let p3 = || acct_f(3, 0.5, 0.3, 150.0, 20.0);
    assert_eq!(go(&from_p1(vec![p3()])), Some((3, Why::Rescue)));
    // a stale reading is no resting target
    assert_eq!(go(&from_p1(vec![Quant { stale: true, ..p3() }])), None);
    // the hourly cap of moves nothing forced is full
    assert_eq!(go(&with(&from_p1(vec![p3()]), |k| k.0 = 4)), None);
    // the pane has made its six resting moves this week
    assert_eq!(go(&with(&from_p1(vec![p3()]), |k| k.1 = 6)), None);
    // not back within six hours of leaving
    let back = |h: f64| {
        let mut x = from_p1(vec![p3()]);
        if let Kind::Rest { left, .. } = &mut x.kind {
            *left = vec![(3, h)];
        }
        go(&x)
    };
    assert_eq!(back(2.0), None);
    assert_eq!(back(7.0), Some((3, Why::Rescue)));
}

#[test]
fn allocation_rescue_leaves_home_when_the_others_there_use_it_up() {
    // P1's Fable renews in 20 h, so it needs this pane unless the other
    // Fable pane there uses up its 0.2 by then
    let at = |other: f64| {
        let a = vec![acct_f(1, 0.5, 0.2, 150.0, 20.0), acct_f(3, 0.5, 0.3, 150.0, 10.0)];
        let tab = [(Some(1), Class::Restricted, 0.02), (Some(1), Class::Restricted, other)];
        let x = resting(a, &tab, FABLE, 1, false, 13.0, 0.05, 0.0);
        assert!(super::shortage(&x.accounts, 0.05));
        go(&x)
    };
    assert_eq!(at(0.001), None, "P1's Fable would lapse without it");
    assert_eq!(at(0.02), Some((3, Why::Rescue)));
}
