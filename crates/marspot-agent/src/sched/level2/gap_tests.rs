//! The gap: demand twice what the accounts hold over the pane horizon,
//! two attended panes and six nobody is at.

use super::route::{Newcomer, RefusedTo, Routing};
use super::tests::{acct, input, pane};
use super::*;

/// Four accounts with 38 points between them, other-model quota only,
/// and eight Opus panes at 0.4 points an hour: 77 points asked in the
/// day.  The attended panes, 1 and 2, sit on the account that empties
/// first and the next.
fn scene() -> (Vec<Account>, Vec<Pane>) {
    let a = vec![acct(1, 0.15, 0.0, 100.0), acct(2, 0.10, 0.0, 100.0), acct(3, 0.08, 0.0, 100.0), acct(4, 0.05, 0.0, 100.0)];
    let on = [4, 3, 1, 1, 2, 2, 3, 4];
    let p = (0..8).map(|k| Pane { attended: k < 2, ..pane(k as u64 + 1, on[k], false, 0.0, 0.004) }).collect();
    (a, p)
}

/// The first refusal of an attended pane, hours, and the last refusal
/// of an unattended one: a pane refused for the last time has stopped
/// for want of quota it may use, so after that moment no unattended
/// pane holds any.
fn refused(plan: &Plan, panes: &[Pane]) -> (Option<f64>, Option<f64>) {
    let at = |attended: bool| plan.refusals.iter().filter(move |(sid, _)| panes.iter().any(|p| p.sid == *sid && p.attended == attended)).map(|r| r.1);
    let first = at(true).fold(None, |m: Option<f64>, h| Some(m.map_or(h, |x| x.min(h))));
    let last = at(false).fold(None, |m: Option<f64>, h| Some(m.map_or(h, |x| x.max(h))));
    (first, last)
}

#[test]
fn sched_gap_balanced_refuses_no_attended_pane_while_others_hold_quota() {
    let (a, p) = scene();
    let mut i = input(&a, &p);
    i.limits = Limits::BALANCED;
    i.pi = i.limits.pi(0.5);
    let bal = plan(&i, None);
    let (first, last) = refused(&bal, &p);
    println!("balanced: attended first refused at {first:?} h, unattended last refused at {last:?} h, refusals {:?}, switches {:?}", bal.refusals, bal.switches.iter().map(|s| (s.sid, s.from, s.to, s.t_open)).collect::<Vec<_>>());
    // every unattended pane has stopped by the time the first attended one is refused
    let first = first.expect("demand twice the supply refuses the attended panes in the end");
    assert!(last.is_some_and(|l| l <= first + 1e-6), "attended refused at {first:.2} h, an unattended pane still worked until {last:?} h");
    let mut s = input(&a, &p);
    s.limits = Limits::STEADY;
    let (first, last) = refused(&plan(&s, None), &p);
    println!("steady, which keeps no quota for anyone: attended first refused at {first:?} h, unattended last refused at {last:?} h");
}

/// Every account down to what only Fable can use, Fable short: under
/// strict protection the unattended other-model panes are the ones that
/// wait, the attended one keeps working, and no other-model newcomer is
/// placed: level 1 lends the plain class nothing of the reserve, so no
/// account has anything for it.  With Fable not short
/// (level 1 finds it cannot use the quota) nobody waits.
#[test]
fn sched_gap_strict_keeps_other_models_off_what_only_fable_can_use() {
    let a = [acct(1, 0.15, 0.3, 100.0), acct(2, 0.10, 0.2, 100.0), acct(3, 0.08, 0.1, 100.0), acct(4, 0.05, 0.1, 100.0)];
    let mut p: Vec<Pane> = (0..8).map(|k| pane(k as u64 + 1, 1 + (k % 4) as u8, k < 4, if k < 4 { 0.004 } else { 0.0 }, if k < 4 { 0.0 } else { 0.004 })).collect();
    p[0].attended = true;
    p[4].attended = true;
    let mut i = input(&a, &p);
    i.limits = Limits::FULL;
    i.pi = i.limits.pi(0.5);
    assert_eq!(guard::waits(&i), vec![6, 7, 8], "the unattended Opus panes wait, the attended one does not");
    let mut r = Routing::of(&i, &plan(&i, None));
    let m = Newcomer { class: crate::quota::Class::Plain, rate: 0.004, high: 0.006, life_h: H2 };
    assert!(r.kept_off_all(m.class), "protection leaves it no account with other-model week");
    assert_eq!(r.clone().place_refused(&m, 4, 8, Some(1)), RefusedTo::Wait, "a refused unattended one waits");
    assert_eq!(r.place(&m, None), None, "nothing lent to other models");
    i.pi = 0.0;
    assert!(guard::waits(&i).is_empty(), "Fable cannot use it all: other models may");
    i.limits = Limits::BALANCED;
    i.pi = i.limits.pi(0.5);
    assert!(guard::waits(&i).is_empty(), "protection without strict makes nobody wait");
}
