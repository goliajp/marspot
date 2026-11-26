use super::*;
use crate::sched::constrain::{PaneNow, keep_feasible};

fn t(money: f64, efficiency: f64) -> Terms {
    Terms { money, efficiency, fairness: 0.0 }
}

#[test]
fn sched_cheapest_wins_whatever_its_efficiency() {
    // two points dearer is more than the week's spending for efficiency allows
    let c = choose(&[(1u8, t(3.0, 0.0)), (2, t(1.0, 3.0))], None, 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(2), Tier::Money));
    assert_eq!(c.runner_up, Some((Some(1), t(3.0, 0.0))));
}

/// A busy pane that was not refused may only be left alone or told: a
/// move that saves five points is not even a candidate.
#[test]
fn sched_interrupting_a_busy_pane_to_save_5_points_chooses_not_to() {
    let mut cands = vec![(Some(3u8), t(-5.0, 0.05))];
    keep_feasible(&mut cands, PaneNow { busy: true, refused: false }, |a| a.is_some());
    assert!(cands.is_empty());
    let c = choose(&cands, Some(t(0.0, 0.0)), 0.0, &POLICY);
    assert_eq!(c.action, None);
    // the same pane refused may move
    let mut cands = vec![(Some(3u8), t(-5.0, 0.05))];
    keep_feasible(&mut cands, PaneNow { busy: true, refused: true }, |a| a.is_some());
    assert_eq!(choose(&cands, Some(t(0.0, 0.0)), 0.0, &POLICY).action, Some(Some(3)));
}

#[test]
fn sched_efficiency_buys_money_within_the_week_cap() {
    let cands = [(1u8, t(0.4, 0.0))];
    let c = choose(&cands, Some(t(0.0, 2.0)), 0.3, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(1), Tier::BoughtByEfficiency));
    assert!((c.week_eff_after - 0.7).abs() < 1e-12);
}

/// Once the week's spending for efficiency is used up, no candidate
/// dearer than the cheapest is chosen, however much better it is.
#[test]
fn sched_once_the_efficiency_week_is_spent_no_dearer_candidate_is_chosen() {
    let cands = [(1u8, t(0.01, 0.0)), (2, t(0.5, 0.0))];
    for spent in [1.0, 0.995] {
        let c = choose(&cands, Some(t(0.0, 100.0)), spent, &POLICY);
        assert_eq!((c.action, c.decided_by, c.week_eff_after), (None, Tier::Money, spent), "spent {spent}");
    }
    // with room left the first one is bought
    let c = choose(&cands, Some(t(0.0, 100.0)), 0.5, &POLICY);
    assert_eq!(c.action, Some(1));
}

#[test]
fn sched_a_move_must_beat_doing_nothing_by_delta() {
    let c = choose(&[(1u8, t(0.0, 0.3))], Some(t(0.0, 0.5)), 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (None, Tier::Efficiency));
    let c = choose(&[(1u8, t(0.0, 0.2))], Some(t(0.0, 0.5)), 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(1), Tier::Efficiency));
}

#[test]
fn sched_ties_go_to_fairness_then_fewer_then_order() {
    let f = |fair: f64| Terms { money: 1.0, efficiency: 0.0, fairness: fair };
    let c = choose(&[(1u8, f(2.0)), (2, f(1.0))], None, 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(2), Tier::Fairness));
    let c = choose(&[(1u8, f(1.0))], Some(f(1.0)), 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (None, Tier::Fewer));
    let c = choose(&[(1u8, f(1.0)), (2, f(1.0))], None, 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(1), Tier::Id));
    let c = choose(&[(1u8, f(1.0))], Some(Terms { money: 1.0 + 1e-7, ..f(1.0) }), 0.0, &POLICY);
    assert_eq!(c.decided_by, Tier::Fewer, "money within float noise is a tie");
}

/// Of the dearer candidates efficiency may buy, the most efficient wins
/// and, among equally efficient ones, the cheapest, whatever the order.
#[test]
fn sched_the_best_buy_is_the_most_efficient_then_the_cheapest() {
    let none = Some(t(0.0, 2.0));
    let c = choose(&[(1u8, t(0.6, 0.0)), (2, t(0.4, 0.0))], none, 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(2), Tier::BoughtByEfficiency));
    assert!((c.week_eff_after - 0.4).abs() < 1e-12);
    let c = choose(&[(1u8, t(0.4, 0.5)), (2, t(0.6, 0.0))], none, 0.0, &POLICY);
    assert_eq!(c.action, Some(2), "efficiency comes before money among buys");
    let f = |money: f64, fairness: f64| Terms { money, efficiency: 0.0, fairness };
    let c = choose(&[(1u8, f(0.4, 2.0)), (2, f(0.4, 1.0))], none, 0.0, &POLICY);
    assert_eq!(c.action, Some(2), "then fairness");
}

/// A money term that is not a number never sets the cheapest and never
/// panics, even when every term is one.
#[test]
fn sched_nan_money_counts_as_dearest() {
    let c = choose(&[(1u8, t(f64::NAN, 0.0)), (2, t(1.0, 0.5))], Some(t(-f64::NAN, 0.0)), 0.0, &POLICY);
    assert_eq!((c.action, c.decided_by), (Some(2), Tier::Money));
    let c = choose(&[(1u8, t(f64::NAN, 0.0))], Some(t(f64::NAN, 0.0)), 0.0, &POLICY);
    assert_eq!((c.action, c.week_eff_after), (None, 0.0));
    let c = choose(&[(1u8, t(f64::NAN, 0.0)), (2, t(f64::NAN, 1.0))], None, 0.0, &POLICY);
    assert_eq!(c.action, Some(1));
}

#[test]
fn sched_nothing_to_choose_is_doing_nothing() {
    let c = choose::<u8>(&[], None, 0.2, &POLICY);
    assert_eq!((c.action, c.runner_up, c.week_eff_after), (None, None, 0.2));
}
