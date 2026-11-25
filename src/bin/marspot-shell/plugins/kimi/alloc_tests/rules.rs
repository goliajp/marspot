use marspot::settings::AgentAllocation;

use super::super::accounts::rooms;
use super::super::alloc::{Decision, KimiAlloc, PaneIn, Snap, quants};
use super::super::alloc_wire::{adapter, rooms_for};
use super::super::extra::{self, Extra, Extras, Paying, Shut};
use super::{NOW, Row, feed, row, scene, situation, would_lines};
use crate::plugins::allocation::replay::Why;
use crate::plugins::claudecode::quota::{Move, next_move};
use crate::plugins::claudecode::seen::Seen;

/// One dry-run or live pass over `text` with `panes`, no refusals.
fn pass(a: &mut KimiAlloc, text: &str, panes: Vec<PaneIn>, now: i64) -> Vec<Decision> {
    let usage = marspot::cc_usage::parse(text).unwrap();
    let extras = extra::parse(text);
    let r = rooms(&[1, 2, 3, 4], Some(&usage), &Seen::default(), now);
    a.observe(&Snap { now, feed_at: Some(usage.generated_at), rooms: &r, extras: &extras, panes })
}

fn resting(sid: u64, helper: bool) -> PaneIn {
    PaneIn { sid, profile: 1, idle: true, helper, out: false }
}

/// P1 has almost nothing left of its week; P3 has most of it.
fn running_out() -> String {
    feed(&[row(1, 0.999, 4), row(3, 0.1, 3)])
}

#[test]
fn kimi_a_helper_gets_no_resting_move() {
    let tmp = marspot_term::test_env::TempDir::new("kimi-alloc-helper-rest");
    // two plain panes: the keeper (sid 7) stays, the other leaves early
    let mut a = KimiAlloc::new(tmp.join("plain"), false);
    let d = pass(&mut a, &running_out(), vec![resting(7, false), resting(8, false)], NOW);
    assert_eq!(d.iter().map(|d| (d.sid, d.to, d.why)).collect::<Vec<_>>(), vec![(8, 3, Why::LeaveEarly)]);
    // the same pane as a helper is planned nothing
    let mut a = KimiAlloc::new(tmp.join("helper"), false);
    let d = pass(&mut a, &running_out(), vec![resting(7, false), resting(8, true)], NOW);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn kimi_a_month_with_less_left_than_the_week_binds_the_plan() {
    // P2: most of its week, 3% of its month (renewing at month end); P3 has half of its week
    let p2 = Row { month: Some(0.97), ..row(2, 0.1, 2) };
    let p3 = Row { month: Some(0.1), ..row(3, 0.5, 3) };
    let text = feed(&[row(1, 0.3, 4), p2, p3, row(4, 0.5, 5)]);
    let usage = marspot::cc_usage::parse(&text).unwrap();
    let extras = extra::parse(&text);
    let r = rooms(&[1, 2, 3, 4], Some(&usage), &Seen::default(), NOW);
    let q = quants(&r, &extras, NOW);
    let at = |id: u8| q.iter().find(|q| q.id == id).unwrap();
    assert!((at(2).r - 0.03).abs() < 1e-9 && (at(2).t - 20.0 * 24.0).abs() < 1e-6, "the month binds P2: {:?}", at(2));
    assert!((at(3).r - 0.5).abs() < 1e-9 && (at(3).t - 3.0 * 24.0).abs() < 1e-6, "the week binds P3: {:?}", at(3));
    assert!((at(4).r - 0.5).abs() < 1e-9, "no month: the week");
    // a pane that has to leave P1 does not go to P2, whose month would not last it six hours
    let tmp = marspot_term::test_env::TempDir::new("kimi-alloc-mixed");
    let mut a = KimiAlloc::new(tmp.path().to_path_buf(), false);
    let d = a.observe(&Snap { now: NOW, feed_at: Some(usage.generated_at), rooms: &r, extras: &extras, panes: vec![PaneIn { out: true, ..resting(7, false) }] });
    assert_eq!(d.iter().map(|d| d.to).collect::<Vec<_>>(), vec![3]);
    // read by its week alone P2 would have been the target
    let week_only = a.observe(&Snap { now: NOW, feed_at: Some(usage.generated_at), rooms: &r, extras: &Extras::new(), panes: vec![PaneIn { out: true, ..resting(7, false) }] });
    assert_eq!(week_only.iter().map(|d| d.to).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn kimi_an_account_seen_paying_stays_shut_through_its_five_hours() {
    // a moment ago P2's five hours were spent with money in its wallet
    let earlier = Extra { five_hour: Some((1.0, Some(NOW + 3_600))), wallet_left: Some(1_000.0), ..Extra::default() };
    let mut paying = Paying::default();
    paying.carry(&mut Extras::from([(2, earlier)]), NOW - 600);
    // now its five hours read 0.1 within the same window: today's rule sends a refused P1 pane there
    let (text, seen) = scene(Row { wallet: Some(1_000.0), ..row(2, 0.2, 2) });
    let usage = marspot::cc_usage::parse(&text).unwrap();
    let mut fresh = extra::parse(&text);
    Paying::default().carry(&mut fresh, NOW);
    assert_eq!(fresh[&2].shut(NOW), None, "without the earlier reading nothing says it pays");
    let today = rooms_for(AgentAllocation::Live, rooms(&[1, 2, 3, 4], Some(&usage), &seen, NOW), &fresh, NOW);
    assert_eq!(next_move(&situation(&today, 1)), Move::To(2));
    let mut extras = extra::parse(&text);
    paying.carry(&mut extras, NOW);
    assert_eq!(extras[&2].shut(NOW), Some(Shut::Paying));
    let live = rooms_for(AgentAllocation::Live, rooms(&[1, 2, 3, 4], Some(&usage), &seen, NOW), &extras, NOW);
    assert!(matches!(next_move(&situation(&live, 1)), Move::To(n) if n != 2), "the wallet keeps it paying");
    // once those five hours renew it is a target again
    let mut later = extra::parse(&text);
    paying.carry(&mut later, NOW + 3_601);
    assert_eq!(later[&2].shut(NOW + 3_601), None);
    // an empty wallet pays nothing
    let mut empty = extra::parse(&feed(&[Row { wallet: Some(0.0), ..row(2, 0.2, 2) }]));
    let mut p = Paying::default();
    p.carry(&mut Extras::from([(2, Extra { wallet_left: Some(1_000.0), rejected: true, ..Extra::default() })]), NOW - 60);
    p.carry(&mut empty, NOW);
    assert_eq!(empty[&2].shut(NOW), None);
}

#[test]
fn kimi_off_drops_the_allocation_and_a_mode_change_rebuilds_it() {
    let tmp = marspot_term::test_env::TempDir::new("kimi-alloc-modes");
    let dir = || tmp.path().to_path_buf();
    let mut slot: Option<KimiAlloc> = None;
    let panes = || vec![resting(7, false), resting(8, false)];
    let a = adapter(&mut slot, AgentAllocation::DryRun, dir).unwrap();
    assert!(!a.live());
    pass(a, &running_out(), panes(), NOW);
    assert_eq!(slot.as_ref().unwrap().last[&8].rest_moves.len(), 1, "the dry run counts its would_move");
    assert_eq!(would_lines(tmp.path()).len(), 1);
    // live counts only `moved` rows: a fresh adapter, nothing counted
    let a = adapter(&mut slot, AgentAllocation::Live, dir).unwrap();
    assert!(a.live());
    pass(a, &running_out(), panes(), NOW + 60);
    assert!(slot.as_ref().unwrap().last[&8].rest_moves.is_empty());
    // back to the dry run: its row is counted again and not written twice
    pass(adapter(&mut slot, AgentAllocation::DryRun, dir).unwrap(), &running_out(), panes(), NOW + 120);
    assert_eq!(slot.as_ref().unwrap().last[&8].rest_moves.len(), 1);
    assert_eq!(would_lines(tmp.path()).len(), 1);
    // off keeps no adapter, so nothing more is written
    assert!(adapter(&mut slot, AgentAllocation::Off, dir).is_none());
    assert!(slot.is_none());
}

#[test]
fn kimi_a_target_that_comes_back_after_no_decision_writes_no_line() {
    let tmp = marspot_term::test_env::TempDir::new("kimi-alloc-gap");
    let mut a = KimiAlloc::new(tmp.path().to_path_buf(), false);
    let text = feed(&[row(1, 0.3, 4), row(3, 0.5, 3)]);
    let out = || vec![PaneIn { out: true, ..resting(7, false) }];
    assert_eq!(pass(&mut a, &text, out(), NOW)[0].to, 3);
    // busy and able to work: no decision, the last target is kept
    assert!(pass(&mut a, &text, vec![PaneIn { idle: false, ..resting(7, false) }], NOW + 60).is_empty());
    assert_eq!(pass(&mut a, &text, out(), NOW + 120)[0].to, 3);
    assert_eq!(would_lines(tmp.path()).len(), 1);
}
