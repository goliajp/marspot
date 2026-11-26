//! Rule (b)'s leave-early, `U` and rescue cases, and the plain pane
//! that waits rather than take the restricted model's share, all through
//! `Inputs::decide` as the shell and the simulator call it.

use super::choose::{MUST_LAST_H, Mover, loads_of};
use super::replay::{Inputs, Kind, Why};
use super::{Class, KAPPA_FABLE, Quant};

pub(super) fn acct(id: u8, r: f64, f: f64, t: f64) -> Quant {
    Quant { id, r, f, t, tf: t, open: true, stale: false }
}

/// An account whose Fable window renews at its own time.
pub(super) fn acct_f(id: u8, r: f64, f: f64, t: f64, tf: f64) -> Quant {
    Quant { tf, ..acct(id, r, f, t) }
}

/// The resting pane `mover` on `here`, `since_h` after its last move.
#[allow(clippy::too_many_arguments)] // one test situation, each a separate piece
pub(super) fn resting(accounts: Vec<Quant>, tab: &[(Option<u8>, Class, f64)], mover: Mover, here: u8, keeper: bool, since_h: f64, b_f: f64, b_n: f64) -> Inputs {
    let kind = Kind::Rest {
        keeper,
        since_move_h: Some(since_h),
        left: Vec::new(),
        band_last_hour: 0,
        demand: b_f + b_n,
        stale_feed: false,
        speed_known: true,
        keeps_restricted: false,
        moves_week: 0,
    };
    Inputs { loads: loads_of(&accounts, tab, None), accounts, b_f, b_n, kappa: Some(KAPPA_FABLE), mover, here, kind }
}

pub(super) fn go(i: &Inputs) -> Option<(u8, Why)> {
    i.decide(&i.plan())
}

pub(super) const PLAIN: Mover = Mover { class: Class::Plain, rate: 0.02, tokens: None, fable: None };

#[test]
fn allocation_leave_early_only_when_it_runs_out_before_renewing() {
    // P1 lasts 1 h at 0.03 an hour; P2 has room for a day
    let tab = [(Some(1), Class::Plain, 0.01), (Some(1), Class::Plain, 0.02)];
    let a = |t1: f64| vec![acct(1, 0.03, 0.0, t1), acct(2, 0.9, 0.0, 60.0)];
    assert_eq!(go(&resting(a(50.0), &tab, PLAIN, 1, false, 5.0, 0.0, 0.03)), Some((2, Why::LeaveEarly)));
    // renewing in half an hour, it never runs out: whatever leaves, lapses
    assert_eq!(go(&resting(a(0.5), &tab, PLAIN, 1, false, 5.0, 0.0, 0.03)), None);
}

#[test]
fn allocation_plain_pane_leaves_when_u_is_gone() {
    // all of P1's week is Fable's (f > R) and Fable is short, so U is 0;
    // P2 has only what plain panes can use
    let a = vec![acct(1, 0.5, 0.52, 100.0), acct(2, 0.5, 0.0, 100.0)];
    let tab = [(Some(1), Class::Restricted, 0.03), (Some(1), Class::Plain, 0.01), (Some(1), Class::Plain, 0.02)];
    let x = resting(a.clone(), &tab, PLAIN, 1, false, 5.0, 0.05, 0.03);
    assert!(x.plan().u[0] < 1e-9, "{:?}", x.plan().u);
    assert_eq!(go(&x), Some((2, Why::UOut)));
    // the slowest plain pane leaves too: what is left is the Fable panes'
    assert_eq!(go(&resting(a.clone(), &tab, Mover { rate: 0.01, ..PLAIN }, 1, true, 5.0, 0.05, 0.03)), Some((2, Why::UOut)));
    // U moves with the Fable demand, so this waits out the cooldown, the
    // hourly cap and the weekly limit like a plan-following move
    assert_eq!(go(&resting(a.clone(), &tab, PLAIN, 1, false, 1.0, 0.05, 0.03)), None);
    assert_eq!(go(&with(&x, |k| k.0 = 4)), None);
    assert_eq!(go(&with(&x, |k| k.1 = 6)), None);
    // with no Fable demand U is all of R and nothing moves
    let none = [(Some(1), Class::Plain, 0.01), (Some(1), Class::Plain, 0.02)];
    assert_eq!(go(&resting(a, &none, PLAIN, 1, false, 5.0, 0.0, 0.03)), None);
}

#[test]
fn allocation_keeper_is_the_slowest_of_its_class() {
    // P1 runs out within two hours: the slowest Fable pane stays even
    // with a slower plain pane there
    let a = vec![acct(1, 0.04, 0.04, 50.0), acct(2, 0.9, 0.5, 60.0)];
    let tab = [(Some(1), Class::Restricted, 0.02), (Some(1), Class::Plain, 0.005)];
    let fable = Mover { class: Class::Restricted, rate: 0.02, tokens: None, fable: None };
    assert_eq!(go(&resting(a.clone(), &tab, fable, 1, true, 5.0, 0.02, 0.005)), None);
    assert_eq!(go(&resting(a, &tab, fable, 1, false, 5.0, 0.02, 0.005)), Some((2, Why::LeaveEarly)));
}

#[test]
fn allocation_plain_pane_waits_rather_than_take_fable_s_share() {
    // refused on P3; P1's week is all Fable's while Fable is short
    let a = vec![acct(1, 0.5, 0.52, 100.0), acct(3, 0.0, 0.0, 50.0)];
    let out = |b_f: f64| Inputs {
        accounts: a.clone(),
        b_f,
        b_n: 0.02,
        kappa: Some(KAPPA_FABLE),
        loads: loads_of(&a, &[], None),
        mover: PLAIN,
        here: 3,
        kind: Kind::Out { horizon_h: MUST_LAST_H },
    };
    assert_eq!(go(&out(0.05)), None, "the rest of P1 is for the Fable panes");
    let x = out(0.05);
    assert_eq!(super::choose::held_back(&x.accounts, &x.plan(), &x.loads, x.mover, Some(3), MUST_LAST_H), Some(1));
    assert_eq!(go(&out(0.0)), Some((1, Why::Out)), "with no Fable demand it is anyone's");
}

/// `x` with its hourly count and weekly count changed by `f`.
pub(super) fn with(x: &Inputs, f: impl Fn(&mut (usize, usize))) -> Inputs {
    let mut y = x.clone();
    if let Kind::Rest { band_last_hour, moves_week, .. } = &mut y.kind {
        let mut k = (*band_last_hour, *moves_week);
        f(&mut k);
        (*band_last_hour, *moves_week) = k;
    }
    y
}

#[test]
fn allocation_fable_pane_judges_each_window_against_its_own_renewal() {
    // a Fable pane at 0.02 an hour alone on P1; P2 lasts it a day
    let fable = Mover { class: Class::Restricted, rate: 0.02, tokens: None, fable: None };
    let tab = [(Some(1), Class::Restricted, 0.02)];
    let go_from = |p1: Quant| go(&resting(vec![p1, acct(2, 0.9, 0.5, 60.0)], &tab, fable, 1, false, 5.0, 0.02, 0.0));
    // Fable runs out in 1.5 h, long before it renews; the week renewing
    // in an hour does not bring it back
    assert_eq!(go_from(acct_f(1, 0.9, 0.03, 1.0, 50.0)), Some((2, Why::LeaveEarly)));
    // the week runs out in 1.5 h, long before it renews; Fable renewing
    // in an hour does not bring it back
    assert_eq!(go_from(acct_f(1, 0.03, 0.5, 50.0, 1.0)), Some((2, Why::LeaveEarly)));
    // Fable renews before it runs out: nothing runs out
    assert_eq!(go_from(acct_f(1, 0.9, 0.03, 50.0, 1.0)), None);
}

#[test]
fn allocation_plain_pane_leaving_early_needs_u_to_last_too() {
    // P1 lasts under an hour; P2's week lasts the pane 25 h, but Fable is
    // short and will take all but 0.1 of it, five hours of this pane
    let a = vec![acct(1, 0.03, 0.0, 50.0), acct(2, 0.5, 0.4, 100.0)];
    let tab = [(Some(1), Class::Plain, 0.02), (Some(1), Class::Plain, 0.01)];
    let x = resting(a.clone(), &tab, PLAIN, 1, false, 5.0, 0.05, 0.03);
    assert!((x.plan().u[1] - 0.1).abs() < 1e-9, "{:?}", x.plan().u);
    assert_eq!(go(&x), None, "it would only have to leave P2 again");
    // with no Fable demand all of P2 is the plain panes'
    assert_eq!(go(&resting(a, &tab, PLAIN, 1, false, 5.0, 0.0, 0.03)), Some((2, Why::LeaveEarly)));
    // leaving early has no weekly limit
    assert_eq!(go(&with(&resting(vec![acct(1, 0.03, 0.0, 50.0), acct(2, 0.9, 0.0, 60.0)], &tab, PLAIN, 1, false, 5.0, 0.0, 0.03), |k| k.1 = 9)), Some((2, Why::LeaveEarly)));
}

#[test]
fn allocation_plain_pane_set_aside_where_fable_has_nothing() {
    // P1's U is gone while Fable is short, nothing lasts the pane nine
    // hours, P2 has a little week left and no Fable: the pane goes there,
    // is refused once that is used, and waits (a planned wait)
    let a = vec![acct(1, 0.5, 0.52, 100.0), acct(2, 0.01, 0.0, 100.0)];
    let tab = [(Some(1), Class::Restricted, 0.03), (Some(1), Class::Plain, 0.02)];
    let x = resting(a.clone(), &tab, PLAIN, 1, true, 5.0, 0.05, 0.02);
    assert_eq!(go(&x), Some((2, Why::UOut)));
    // a pane using Fable through its agents is not sent where Fable is spent
    let mut keeps = x.clone();
    if let Kind::Rest { keeps_restricted, .. } = &mut keeps.kind {
        *keeps_restricted = true;
    }
    assert_eq!(go(&keeps), None);
    // with Fable not short, U is all of P1's week and nothing moves
    assert_eq!(go(&resting(a, &tab, PLAIN, 1, true, 5.0, 0.004, 0.02)), None);
}

#[test]
fn allocation_reservation_counts_the_plain_panes_already_there() {
    // refused on P3; P1's N (0.15) lasts this pane 7.5 h alone but 3.75 h
    // with the plain pane already there, while Fable is short
    let a = vec![acct(1, 0.5, 0.35, 100.0), acct(3, 0.0, 0.0, 50.0)];
    let out = |tab: &[(Option<u8>, Class, f64)]| Inputs {
        accounts: a.clone(),
        b_f: 0.05,
        b_n: 0.04,
        kappa: Some(KAPPA_FABLE),
        loads: loads_of(&a, tab, None),
        mover: PLAIN,
        here: 3,
        kind: Kind::Out { horizon_h: MUST_LAST_H },
    };
    assert_eq!(go(&out(&[])), Some((1, Why::Out)));
    let x = out(&[(Some(1), Class::Plain, 0.02)]);
    assert_eq!(go(&x), None);
    let p = x.plan();
    assert_eq!(super::choose::held_back(&x.accounts, &p, &x.loads, x.mover, Some(3), MUST_LAST_H), Some(1));
    assert_eq!(super::choose::held_back(&a, &p, &loads_of(&a, &[], None), PLAIN, Some(3), MUST_LAST_H), None, "it is placed");
}

#[test]
fn allocation_u_reservation_counts_the_plain_panes_already_there() {
    // Fable is short and takes all of P2 but its N, 0.02: that lasts a
    // pane at 0.01 two hours alone, 40 minutes beside one at 0.02
    let a = vec![acct(2, 0.5, 0.48, 100.0), acct(3, 0.0, 0.0, 50.0)];
    let mover = Mover { class: Class::Plain, rate: 0.01, tokens: None, fable: None };
    let x = resting(a.clone(), &[], mover, 3, false, 5.0, 0.05, 0.03);
    let p = x.plan();
    assert!(p.unassigned_f > 0.0 && (p.u[0] - 0.02).abs() < 1e-9, "{p:?}");
    let pick = |tab: &[(Option<u8>, Class, f64)]| super::choose::pick_where(&a, &p, &loads_of(&a, tab, None), mover, Some(3), 0.0, super::choose::Reserve::U, |_| true);
    assert_eq!(pick(&[]), Some(2));
    assert_eq!(pick(&[(Some(2), Class::Plain, 0.02)]), None);
}
