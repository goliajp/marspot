//! The credits rule when a move does not happen, when a week is used up
//! before any credits are seen spent, and when a target is too short.

use super::credits::Acct;
use super::stop_paying::{need, target, Log, Mover, StopPaying, MIN_LEFT, MOVES_PER_HOUR, RETRY_S};
use super::stop_paying_tests::{acct, credit, fleet, mv, ok, plugin, sandbox, Host, CODEX, H, LAST, NOW, SID, THREAD};
use crate::plugins::peerbus::moving::{self, Ask, Released, Verdict};
use crate::plugins::pty_op;
use marspot::settings::Settings;

fn held_lines(log: &Log) -> Vec<&str> {
    log.iter().filter(|(t, _)| *t == "quota.stop_paying.held").map(|(_, l)| l.as_str()).collect()
}

#[test]
fn a_used_up_week_with_credits_moves_before_any_fall() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    let accts = vec![acct(1, 0.09, false), acct(2, 1.0, false)];
    let o = s.decide(LAST, &accts, NOW, mv(SID, 2), ok, &mut log).expect("ordered without a falling balance");
    assert_eq!((o.from, o.to), (2, 1));
    assert_eq!(log, [("quota.stop_paying", "sid=487 from=P2 to=P1 balance_before=46434.38 plan_left_to=0.91 reason=exhausted".to_string())]);
}

#[test]
fn one_pane_refused_again_and_again_leaves_the_hour_to_the_others() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    let read_at = |t: i64| -> Vec<Acct> { fleet().into_iter().map(|a| Acct { at: t, ..a }).collect() };
    let mut now = NOW;
    for _ in 0..6 {
        assert!(s.decide(LAST, &read_at(now), now, mv(7, 2), ok, &mut log).is_some(), "ordered at {now}");
        s.undo(7, &mut log, "queued");
        assert_eq!(s.decide(LAST, &read_at(now), now + 60, mv(7, 2), || panic!("not asked within RETRY_S"), &mut log), None);
        now += RETRY_S;
    }
    assert_eq!(held_lines(&log), ["sid=7 from=P2 why=queued"], "said once, not on every refusal");
    for sid in 100..100 + MOVES_PER_HOUR as u64 {
        assert!(s.decide(LAST, &read_at(now), now, mv(sid, 2), ok, &mut log).is_some(), "pane {sid} still has its place");
    }
}

#[test]
fn a_move_that_began_and_failed_is_said_once_and_its_after_is_dropped() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    s.decide(LAST, &fleet(), NOW, mv(SID, 2), ok, &mut log).expect("ordered");
    log.clear();
    s.failed(SID, &mut log, "still_loaded");
    assert_eq!(held_lines(&log), ["sid=487 from=P2 why=still_loaded"]);
    for t in [NOW + 60, NOW + RETRY_S, NOW + H] {
        assert_eq!(s.decide(LAST, &fleet(), t, mv(SID, 2), || panic!("not asked"), &mut log), None, "once per used-up week");
    }
    assert_eq!(held_lines(&log).len(), 1, "the failure stays the reason: {log:?}");
    let mut later = fleet();
    later[1].balance = Some((NOW + 300, 46420.0));
    log.clear();
    s.follow_afters(&later, NOW + 300, &mut log);
    assert!(log.is_empty(), "no after for a move that did not happen: {log:?}");
}

#[test]
fn a_target_must_cover_the_minimum_stay_at_the_panes_rate() {
    assert_eq!(need(3.0, 0.001), MIN_LEFT, "never under the floor");
    assert!((need(3.0, 0.05) - 0.15).abs() < 1e-9);
    let accts = vec![acct(1, 0.90, false), acct(2, 1.0, true)];
    assert_eq!(target(&accts, 2, NOW, 0.05).map(|a| a.profile), Some(1));
    assert_eq!(target(&accts, 2, NOW, 0.15), None, "10% left is not three hours at 5% an hour");
    let mut s = StopPaying::default();
    let mut log = Log::new();
    let m = Mover { sid: SID, from: 2, need: 0.15 };
    assert_eq!(s.decide(LAST, &accts, NOW, m, || panic!("not asked"), &mut log), None);
    assert_eq!(held_lines(&log), ["sid=487 from=P2 why=no_target"]);
}

#[test]
fn a_switch_whose_submit_fails_is_logged_and_not_ordered_again_that_week() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    marspot::settings::set_for_test(Settings::default());
    let (mut p, asks) = plugin(&home);
    let host = Host { refuse: true, ..Default::default() };
    p.stop_paying(&host, credit(NOW));
    assert_eq!(asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }));
    let old = home.path().join(".codex-profile-2");
    moving::post(SID, CODEX, Verdict::Go { thread: THREAD.into(), home: old, released: Released(pty_op::Job::new()) });
    p.follow_switches(&host);
    let said = |tag: &str, line: &str| host.logs.lock().unwrap().iter().filter(|(t, m)| t == tag && m == line).count();
    assert_eq!(said("quota.stop_paying.held", "sid=487 from=P2 why=switch_failed"), 1);
    p.stop_paying(&host, credit(NOW + H / 2));
    assert!(!std::iter::from_fn(|| asks.next()).any(|a| matches!(a, Ask::Begin { .. })), "not ordered again this week");
    assert_eq!(said("quota.stop_paying.held", "sid=487 from=P2 why=switch_failed"), 1, "said once");
    assert_eq!(host.logs.lock().unwrap().iter().filter(|(t, _)| t == "quota.stop_paying").count(), 1);
}
