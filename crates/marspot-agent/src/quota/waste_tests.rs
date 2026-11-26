//! The one objective: the forecast, where a pane that has to move goes,
//! when a resting pane moves, and a helper's default model.

use super::choose::{HELPER_LAST_H, MUST_LAST_H, Mover};
use super::replay::{Inputs, Kind, Rule, Why, replay};
use super::waste::{self, PARAMS, Params, decide, forecast, pick, restricted_helper};
use super::{Class, KAPPA_FABLE, Load, Quant};

fn acct(id: u8, r: f64, fable_used: f64, t: f64) -> Quant {
    Quant { id, r, f: KAPPA_FABLE * (1.0 - fable_used), t, tf: t, open: true, stale: false }
}

fn load(restricted: f64, plain: f64) -> Load {
    Load { restricted, plain }
}

const PLAIN: Mover = Mover { class: Class::Plain, rate: 0.0165, tokens: None };
const FABLE: Mover = Mover { class: Class::Restricted, rate: 0.006, tokens: None };

fn out(accounts: &[Quant], loads: &[Load], m: Mover, from: u8) -> Option<u8> {
    pick(accounts, loads, m, Some(from), Some(KAPPA_FABLE), MUST_LAST_H, &PARAMS, |_| true)
}

#[test]
fn alloc_waste_forecast_counts_the_week_and_fable_left() {
    // nothing on it: all of both lapses at the renewal
    let q = acct(1, 0.5, 0.0, 50.0);
    let o = forecast(&q, &Load::default(), None, Some(KAPPA_FABLE), 50.0);
    assert!((o.w7 - 0.5).abs() < 1e-9 && (o.wf - KAPPA_FABLE).abs() < 1e-9 && o.out_plain.is_none(), "{o:?}");
    // plain use fills the week in 20 h and strands the Fable left
    let o = forecast(&q, &load(0.0, 0.025), None, Some(KAPPA_FABLE), 50.0);
    assert!(o.w7.abs() < 1e-9 && (o.wf - KAPPA_FABLE).abs() < 1e-9, "{o:?}");
    assert!((o.out_plain.unwrap() - 20.0).abs() < 1e-6 && (o.out_restricted.unwrap() - 20.0).abs() < 1e-6, "{o:?}");
    // a Fable pane is refused once its window is spent, before the week
    let q = acct(2, 0.8, 0.5, 60.0);
    let o = forecast(&q, &load(0.01, 0.0), None, Some(KAPPA_FABLE), 60.0);
    assert!((o.out_restricted.unwrap() - 26.4).abs() < 1e-6 && o.out_plain.is_none() && o.wf.abs() < 1e-9, "{o:?}");
    // the extra pane only for the hours it is assumed to stay
    let o = forecast(&acct(3, 1.0, 1.0, 100.0), &Load::default(), Some((PLAIN, 10.0)), Some(KAPPA_FABLE), 100.0);
    assert!((o.w7 - (1.0 - 0.165)).abs() < 1e-9, "{o:?}");
    // every renewal inside the span counts, a fresh week and window after each
    let o = forecast(&acct(4, 0.5, 0.0, 50.0), &Load::default(), None, Some(KAPPA_FABLE), 218.0);
    assert!((o.w7 - 1.5).abs() < 1e-9 && (o.wf - 2.0 * KAPPA_FABLE).abs() < 1e-9, "{o:?}");
    // nothing known of the window's worth: no restricted waste
    assert_eq!(forecast(&q, &Load::default(), None, None, 60.0).wf, 0.0);
}

/// An account renewing early is not cheaper to place a pane on because
/// its next week falls outside what is counted: two accounts with the
/// same room that the pane cannot use up cost the same.
#[test]
fn alloc_waste_compares_accounts_over_one_span() {
    let m = Mover { class: Class::Plain, rate: super::RHO0, tokens: None };
    let early = waste::added(&acct(1, 0.9, 0.0, 50.0), &Load::default(), m, Some(KAPPA_FABLE), MUST_LAST_H, &PARAMS);
    let late = waste::added(&acct(2, 0.9, 0.0, 168.0), &Load::default(), m, Some(KAPPA_FABLE), MUST_LAST_H, &PARAMS);
    assert!((early.cost - late.cost).abs() < 1e-9 && early.cost < 0.0, "{early:?} {late:?}");
}

/// A move's cost grows with the context it rewrites; unknown is the
/// typical one.
#[test]
fn alloc_waste_switch_cost_follows_the_context() {
    assert_eq!(waste::switch_cost(Class::Restricted, None), waste::SWITCH_RESTRICTED);
    assert_eq!(waste::switch_cost(Class::Plain, None), waste::SWITCH_PLAIN);
    assert!((waste::switch_cost(Class::Restricted, Some(230_000)) - waste::SWITCH_RESTRICTED / 2.0).abs() < 1e-15);
    assert!((waste::switch_cost(Class::Plain, Some(920_000)) - 2.0 * waste::SWITCH_PLAIN).abs() < 1e-15);
}

/// The 2026-11-25 02:10Z refusal of the coordinator's plain pane on P2:
/// P4's week is what its Fable panes will need, P7's is only the plain
/// models', and P3's Fable will lapse with no Fable pane on it.
fn nov25() -> (Vec<Quant>, Vec<Load>) {
    let accounts = vec![acct(2, 0.0, 1.0, 115.0), acct(3, 0.5, 0.0, 75.0), acct(4, 0.8, 0.31, 61.0), acct(6, 0.42, 1.0, 27.0), acct(7, 0.44, 1.0, 122.0)];
    let loads = vec![load(0.0, 0.01), Load::default(), load(0.0064, 0.0), load(0.0, 0.022), Load::default()];
    (accounts, loads)
}

#[test]
fn alloc_waste_refused_plain_pane_spares_fable_room_when_there_is_room_elsewhere() {
    let (a, l) = nov25();
    // P4's Fable panes will use 36 of its 80 points before it renews;
    // the pane, in its longest stay, uses less than the 44 they leave:
    // P4, the earliest renewal among accounts where all it uses lapses
    assert_eq!(out(&a, &l, PLAIN, 2), Some(4));
    // one twice as fast would eat into what the Fable panes need: P3,
    // whose week lapses and lasts it
    let fast = Mover { class: Class::Plain, rate: 0.02, tokens: None };
    assert_eq!(out(&a, &l, fast, 2), Some(3));
    // P4 against P7 alone: P7, whose week only the plain models can use
    let two = vec![a[0].clone(), a[2].clone(), a[4].clone()];
    assert_eq!(out(&two, &[l[0], l[2], l[4]], fast, 2), Some(7));
    // only P4 left: it still goes there, nothing is held back
    let only = vec![a[0].clone(), a[2].clone()];
    assert_eq!(out(&only, &[l[0], l[2]], fast, 2), Some(4));
}

#[test]
fn alloc_waste_refused_order_is_by_cost_then_renewal() {
    // equal accounts nothing uses, each lasting past the longest stay:
    // the earlier renewal first, then the number
    let a = vec![acct(1, 0.0, 1.0, 10.0), acct(2, 0.9, 1.0, 150.0), acct(3, 0.9, 1.0, 100.0), acct(4, 0.9, 1.0, 100.0)];
    let l = vec![Load::default(); 4];
    assert_eq!(out(&a, &l, Mover { class: Class::Plain, rate: 0.005, tokens: None }, 1), Some(3));
    // one renewing much sooner saves the same over the span: the earlier
    let a = vec![acct(1, 0.0, 1.0, 10.0), acct(2, 0.9, 1.0, 150.0), acct(3, 0.9, 1.0, 30.0)];
    assert_eq!(out(&a, &l[..3], Mover { class: Class::Plain, rate: 0.005, tokens: None }, 1), Some(3));
    // an account that would refuse it within six hours comes after any
    // that would not
    let a = vec![acct(1, 0.0, 1.0, 10.0), acct(2, 0.02, 1.0, 20.0), acct(3, 0.3, 1.0, 150.0)];
    let l = vec![Load::default(); 3];
    assert_eq!(out(&a, &l, Mover { class: Class::Plain, rate: 0.01, tokens: None }, 1), Some(3), "P2 would refuse it in two hours");
    // nor one where taking it leaves the Fable pane there refused within
    // six hours too: P2's
    // Fable pane lasts eight hours alone and four with the plain pane
    let a = vec![acct(1, 0.0, 1.0, 10.0), acct(2, 0.08, 0.0, 100.0), acct(3, 0.3, 1.0, 150.0)];
    let l = vec![Load::default(), load(0.01, 0.0), Load::default()];
    assert_eq!(out(&a, &l, Mover { class: Class::Plain, rate: 0.01, tokens: None }, 1), Some(3));
    // the only one left still takes it: nothing is held back
    assert_eq!(out(&a[..2], &l[..2], Mover { class: Class::Plain, rate: 0.01, tokens: None }, 1), Some(2));
}

/// A refused busy pane is not sent where a large restricted window on
/// a small week would refuse it again within the horizon.
#[test]
fn alloc_waste_refused_pane_avoids_an_account_that_refuses_it_soon() {
    // P2: most of its Fable left but only five points of week
    let a = vec![acct(1, 0.0, 1.0, 10.0), acct(2, 0.05, 0.0, 100.0), acct(3, 0.5, 0.9, 150.0)];
    let l = vec![Load::default(); 3];
    assert_eq!(out(&a, &l, Mover { class: Class::Restricted, rate: 0.01, tokens: None }, 1), Some(3));
}

#[test]
fn alloc_waste_fable_pane_goes_where_most_fable_would_lapse() {
    // P4's Fable panes already use its window up; P3 and P8 would lapse,
    // and on P8 the pane's longest stay would outrun the window, which on
    // P3 renews before it does
    let a = vec![acct(1, 0.0, 1.0, 30.0), acct(3, 0.46, 0.0, 74.0), acct(4, 0.5, 0.6, 36.0), acct(8, 0.98, 0.03, 146.0)];
    let l = vec![Load::default(), Load::default(), load(0.006, 0.0), Load::default()];
    assert_eq!(out(&a, &l, FABLE, 1), Some(3));
    // without P3, P8: P4's window is its own panes'
    let no3 = vec![a[0].clone(), a[2].clone(), a[3].clone()];
    assert_eq!(out(&no3, &[l[0], l[2], l[3]], FABLE, 1), Some(8));
    // two windows that both outlast its use: the earlier renewal
    let b = vec![acct(1, 0.0, 1.0, 30.0), acct(4, 1.0, 0.0, 100.0), acct(5, 1.0, 0.0, 140.0)];
    assert_eq!(out(&b, &[Load::default(); 3], FABLE, 1), Some(4));
    // a Fable pane is never sent where Fable is spent, nor switched to another model
    let spent = vec![acct(1, 0.0, 1.0, 30.0), acct(7, 0.44, 1.0, 120.0)];
    assert_eq!(out(&spent, &[Load::default(); 2], FABLE, 1), None);
}

fn resting(accounts: Vec<Quant>, loads: Vec<Load>, m: Mover, here: u8, since_h: f64, moves_week: usize) -> Inputs {
    let kind = Kind::Rest {
        keeper: false,
        since_move_h: Some(since_h),
        left: Vec::new(),
        band_last_hour: 0,
        demand: 0.0,
        stale_feed: false,
        speed_known: true,
        keeps_restricted: false,
        moves_week,
    };
    Inputs { accounts, b_f: 0.0, b_n: 0.0, kappa: Some(KAPPA_FABLE), loads, mover: m, here, kind }
}

fn rest_with(i: &Inputs, day: usize, p: &Params) -> Option<(u8, Why)> {
    decide(&i.accounts, &i.loads, i.mover, i.here, i.kappa, &i.kind, day, p)
}

#[test]
fn alloc_waste_resting_plain_pane_leaves_the_fable_panes_week() {
    // P4 late on 11-25: the plain pane's use would fill the week before
    // the Fable panes have used their window; P7 has room only it can use
    let a = vec![acct(4, 0.2, 0.55, 40.0), acct(7, 0.44, 1.0, 120.0)];
    let l = vec![load(0.006, 0.0165), Load::default()];
    let i = resting(a.clone(), l.clone(), PLAIN, 4, 5.0, 0);
    assert_eq!(rest_with(&i, 0, &PARAMS), Some((7, Why::Waste)));
    // within its cooldown, past the week's or the day's moves: it stays
    assert_eq!(rest_with(&resting(a.clone(), l.clone(), PLAIN, 4, 1.0, 0), 0, &PARAMS), None);
    assert_eq!(rest_with(&resting(a.clone(), l.clone(), PLAIN, 4, 5.0, 6), 0, &PARAMS), None);
    assert_eq!(rest_with(&i, PARAMS.per_day, &PARAMS), None);
    // a margin larger than what it saves keeps it home
    assert_eq!(rest_with(&i, 0, &Params { margin: 1.0, ..PARAMS }), None);
    // nothing to save: a pane alone on an account with more to lapse stays
    let calm = vec![acct(4, 0.9, 0.0, 120.0), acct(7, 0.44, 1.0, 40.0)];
    assert_eq!(rest_with(&resting(calm, vec![load(0.0, 0.005), Load::default()], Mover { class: Class::Plain, rate: 0.005, tokens: None }, 4, 5.0, 0), 0, &PARAMS), None);
}

/// The same work on Fable costs about four times the week.
const FABLE_WEEK: f64 = 4.0;

#[test]
fn alloc_waste_helper_runs_fable_only_when_fable_would_lapse() {
    let rho = super::RHO0;
    // a helper runs a couple of hours
    let p = Params { stay_h: 2.0, ..PARAMS };
    let helper = |a: &[Quant], l: &[Load], kappa| restricted_helper(a, l, rho, rho * FABLE_WEEK, kappa, HELPER_LAST_H, &p);
    // P3's week and Fable lapse with nothing on them: what a Fable helper
    // uses there four times as fast takes nothing from anyone
    let a = vec![acct(3, 0.46, 0.0, 74.0), acct(7, 0.44, 1.0, 120.0)];
    assert!(helper(&a, &[Load::default(); 2], Some(KAPPA_FABLE)));
    // P3's week nearly gone: a Fable helper would be refused there within
    // hours, a plain one on P7 would not
    let short = vec![acct(3, 0.02, 0.0, 74.0), acct(7, 0.44, 1.0, 120.0)];
    assert!(!helper(&short, &[Load::default(); 2], Some(KAPPA_FABLE)));
    // P3's Fable panes use all of it: a plain helper on P7 saves more
    let busy = vec![load(0.02, 0.0), Load::default()];
    assert!(!helper(&a, &busy, Some(KAPPA_FABLE)));
    // P3's Fable would lapse but its week would not: what a Fable helper
    // eats of the week is what P3's plain panes lose, a plain helper on
    // P7 uses a week that lapses
    let plain_on_3 = vec![load(0.0, 0.0062), Load::default()];
    assert!(!helper(&a, &plain_on_3, Some(KAPPA_FABLE)));
    // no Fable anywhere, or nothing known of it: the default model
    let spent = vec![acct(7, 0.44, 1.0, 120.0)];
    assert!(!helper(&spent, &[Load::default()], Some(KAPPA_FABLE)));
    assert!(!helper(&a, &[Load::default(); 2], None));
}

#[test]
fn alloc_waste_logged_line_replays_by_its_rule() {
    let (a, l) = nov25();
    let i = Inputs { accounts: a, b_f: 0.0064, b_n: 0.05, kappa: Some(KAPPA_FABLE), loads: l, mover: PLAIN, here: 2, kind: Kind::Out { horizon_h: MUST_LAST_H } };
    let rule = Rule::waste(3);
    let line = format!("to=P7 why=out {}{}", i.encode(), rule.words());
    assert_eq!(Rule::of_line(&line), rule);
    let to = waste::pick(&i.accounts, &i.loads, i.mover, Some(2), i.kappa, MUST_LAST_H, &PARAMS, |_| true);
    assert_eq!(replay(&line), Some(to.map(|t| (t, Why::Out))));
    // a line without the rule replays by the bands, as it was written
    assert_eq!(replay(&i.encode()), Some(i.decide(&i.plan())));
}
