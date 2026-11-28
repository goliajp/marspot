use super::super::tests::{KAPPA, q, world};
use super::MARGIN_H;
use super::super::*;
use crate::sched::reset::{Supply, WeekCut};

fn window_reset(ends_h: Option<f64>) -> Supply {
    Supply { week: false, window: true, gives_back_week: true, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h, left: 1, usable_below_left: None }
}

/// P3 of 11-24: restricted window spent, renewal two days away; a
/// window reset that needs the limit is claimed when the window empties.
#[test]
fn sched_level1_a_window_reset_is_claimed_where_restricted_demand_waits() {
    let a = [q(3, 0.3, 0.02, 60.0)];
    let offers = [Offer { account: 3, supply: window_reset(None) }];
    let w = World { offers: &offers, ..world(&a, [0.01, 0.0], 0.01) };
    let p = plan(&w);
    assert_eq!(p.claims.len(), 1, "{:?}", p.claims);
    let c = p.claims[0];
    // 0.02 at 0.01 an hour: the window empties at 2 h
    assert!((c.at_h - 2.0).abs() < 1e-9 && c.account == 3 && c.offer == 0, "{c:?}");
    assert!(c.value > 0.2, "{c:?}");
    assert_eq!(p.combos, 2);
    let without = plan(&world(&a, [0.01, 0.0], 0.01));
    assert!(p.served[0] > without.served[0] + 0.2);
}

#[test]
fn sched_level1_no_reset_is_claimed_within_six_hours_of_the_renewal() {
    // the window empties at 2 h, the week renews at 7 h
    let a = [q(3, 0.3, 0.02, 7.0)];
    let offers = [Offer { account: 3, supply: window_reset(None) }];
    let p = plan(&World { offers: &offers, ..world(&a, [0.01, 0.0], 0.01) });
    assert!(p.claims.is_empty(), "{:?}", p.claims);
}

#[test]
fn sched_level1_a_reset_ending_before_the_limit_is_reached_is_not_claimed_and_one_unneeded_is_not_lost() {
    let a = [q(3, 0.3, 0.02, 60.0)];
    let offers = [Offer { account: 3, supply: window_reset(Some(1.0)) }];
    let p = plan(&World { offers: &offers, ..world(&a, [0.01, 0.0], 0.01) });
    assert!(p.claims.is_empty() && p.money.offers_lost == 0.0, "{:?} {:?}", p.claims, p.money);
    // plenty of restricted quota: a reset serves nothing
    let a = [q(3, 1.0, KAPPA, 60.0)];
    let offers = [Offer { account: 3, supply: window_reset(None) }];
    let p = plan(&World { offers: &offers, ..world(&a, [0.001, 0.0], 0.001) });
    assert!(p.claims.is_empty() && p.combos == 1);
}

/// With resets no optimality is promised: the claim time is compared
/// with every boundary of the plan without claims, and the gap reported.
#[test]
fn sched_level1_the_gap_of_the_claim_time_is_reported() {
    let a = [q(3, 0.3, 0.02, 60.0), q(4, 0.1, 0.1, 30.0)];
    let offers = [Offer { account: 3, supply: Supply { requires_limit: false, ends_h: Some(40.0), ..window_reset(None) } }];
    let w = World { offers: &offers, ..world(&a, [0.01, 0.002], 0.01) };
    let p = plan(&w);
    let base = super::super::items::items_of(w.accounts, w.window_full);
    let s = solve(&w, base.clone());
    let mut best = p.served[0] + p.served[1];
    let mut at_best = None;
    for &at in s.grid.cuts.iter().filter(|&&c| c < 40.0) {
        let o = [Offer { account: 3, supply: Supply { ends_h: Some(at + MARGIN_H), ..offers[0].supply } }];
        let ww = World { offers: &o, ..world(&a, [0.01, 0.002], 0.01) };
        let got = plan(&ww).served;
        if got[0] + got[1] > best + 1e-12 {
            (best, at_best) = (got[0] + got[1], Some(at));
        }
    }
    let gap = best - p.served[0] - p.served[1];
    eprintln!("claim at {:?}, best at {at_best:?}, gap {:.4} points", p.claims.first().map(|c| c.at_h), gap * 100.0);
    assert!(gap >= 0.0 && p.claims.len() == 1);
}

/// Claude's two stored resets, usable any time before they end in 40 h:
/// one brings back the week and the window, the other the window and
/// half of what the week used.
fn claude_full() -> Supply {
    Supply { week: true, window: true, gives_back_week: false, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: false, starts_h: 0.0, ends_h: Some(40.0), left: 1, usable_below_left: None }
}

fn claude_fable() -> Supply {
    Supply { week: false, window: true, week_cut: WeekCut::ShareOfUsed(0.5), ..claude_full() }
}

#[test]
fn sched_level1_where_the_window_binds_the_window_reset_is_claimed_and_the_full_one_kept() {
    // the window is spent in 2 h, the week has plenty
    let a = [q(3, 0.9, 0.02, 60.0)];
    let offers = [Offer { account: 3, supply: claude_full() }, Offer { account: 3, supply: claude_fable() }];
    let p = plan(&World { offers: &offers, ..world(&a, [0.01, 0.0], 0.01) });
    assert_eq!(p.claims.len(), 1, "{:?}", p.claims);
    assert_eq!(p.claims[0].offer, 1, "the fable reset serves as much: {:?}", p.claims);
}

#[test]
fn sched_level1_where_the_week_binds_the_full_reset_is_claimed() {
    // plain demand only; the week is spent in 5 h
    let a = [q(3, 0.05, 0.05, 100.0)];
    let offers = [Offer { account: 3, supply: claude_fable() }, Offer { account: 3, supply: claude_full() }];
    let p = plan(&World { offers: &offers, ..world(&a, [0.0, 0.01], 0.0) });
    assert_eq!(p.claims.len(), 1, "{:?}", p.claims);
    assert_eq!(p.claims[0].offer, 1, "half the week is less than a whole one: {:?}", p.claims);
    let fable_only = plan(&World { offers: &offers[..1], ..world(&a, [0.0, 0.01], 0.0) });
    assert_eq!(fable_only.claims.len(), 1, "alone it still brings back half the week: {:?}", fable_only.claims);
    assert!(p.served[1] > fable_only.served[1] + 0.05, "{:?} {:?}", p.served, fable_only.served);
}
