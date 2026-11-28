//! The credits rule when a move does not happen, when a week is used up
//! before any credits are seen spent, and when a target is too short;
//! and the rule over a plugin: peerbus's verdict, the switch it starts,
//! the settings file, and what the pane's screen shows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use super::credits::Acct;
use super::pane::PaneCodex;
use super::stop_paying::{need, target, Log, Mover, StopPaying, MIN_LEFT, MOVES_PER_HOUR, RETRY_S};
use super::stop_paying_screen::asks_approval;
use super::stop_paying_tests::{acct, credit, fleet, mv, ok, H, LAST, NOW};
use super::CodexPlugin;
use crate::plugins::peerbus::moving::{self, Ask, Held, Listener, Released, Verdict};
use crate::plugins::peerbus::{publish_threads, ThreadView};
use crate::plugins::{pty_op, LogLevel, PaneStatusView, PluginError, PluginHost};
use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcId;
use marspot::settings::{SchedCredits, Settings};
use marspot_term::test_env::{EnvGuard, TempDir};

/// What peerbus knows of a pane's thread, as the plugin's pass reads it.
fn bound(sid: u64, codex: ProcId) -> Option<bool> {
    crate::plugins::peerbus::threads().get(&sid).filter(|v| v.codex == codex).map(|v| v.idle)
}

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
    p.stop_paying(&host, credit(NOW), bound);
    assert_eq!(asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }));
    let old = home.path().join(".codex-profile-2");
    moving::post(SID, CODEX, Verdict::Go { thread: THREAD.into(), home: old, released: Released(pty_op::Job::new()) });
    p.follow_switches(&host);
    let said = |tag: &str, line: &str| host.logs.lock().unwrap().iter().filter(|(t, m)| t == tag && m == line).count();
    assert_eq!(said("quota.stop_paying.held", "sid=487 from=P2 why=switch_failed"), 1);
    p.stop_paying(&host, credit(NOW + H / 2), bound);
    assert!(!std::iter::from_fn(|| asks.next()).any(|a| matches!(a, Ask::Begin { .. })), "not ordered again this week");
    assert_eq!(said("quota.stop_paying.held", "sid=487 from=P2 why=switch_failed"), 1, "said once");
    assert_eq!(host.logs.lock().unwrap().iter().filter(|(t, _)| t == "quota.stop_paying").count(), 1);
}

#[derive(Default)]
struct Host {
    pub logs: Mutex<Vec<(String, String)>>,
    pub submitted: Mutex<Vec<&'static str>>,
    /// Every submit is refused, as by a pane that went away.
    pub refuse: bool,
}

impl PluginHost for Host {
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        Ok(std::env::temp_dir())
    }
    fn log(&self, _l: LogLevel, t: &str, m: &str) {
        self.logs.lock().unwrap().push((t.to_string(), m.to_string()));
    }
    fn submit_pty_op_at(&self, _sid: u64, op: pty_op::PtyOp, _start_at: usize) -> Result<(), PluginError> {
        if self.refuse {
            return Err(PluginError::Other("no such pane".into()));
        }
        self.submitted.lock().unwrap().push(op.name);
        Ok(())
    }
    fn pane_status(&self, _sid: u64) -> Result<Option<PaneStatusView>, PluginError> {
        let held = Duration::from_secs(60);
        Ok(Some(PaneStatusView { status: PaneStatus::Busy(BusyKind::ForegroundJob), held, quiescent: true }))
    }
}

const SID: u64 = 487;
const CODEX: ProcId = ProcId { pid: i32::MAX, start_us: 1 };
const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

/// A codex pane on profile 2, bound idle to its thread, with an empty
/// input box, and a worker that takes asks without answering them.
fn plugin(home: &TempDir) -> (CodexPlugin, Listener) {
    let mut p = CodexPlugin::new();
    p.paying.looks.screen = |_| None;
    p.paying.looks.composer = |_| Some(String::new());
    p.panes.insert(SID, PaneCodex { codex: CODEX, shell_pid: i32::MAX, profile: Some(2), argv: Some(vec!["codex".into()]) });
    let home2 = home.path().join(".codex-profile-2");
    publish_threads(HashMap::from([(SID, ThreadView { codex: CODEX, home: home2, thread: THREAD.into(), idle: true, ends: Default::default() })]));
    (p, Listener::connect())
}

fn sandbox() -> (TempDir, EnvGuard) {
    let home = TempDir::new("codex-stop-paying");
    for d in [".codex-profile-1", ".codex-profile-2"] {
        std::fs::create_dir_all(home.path().join(d)).unwrap();
    }
    let mut env = EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", home.join("state"));
    (home, env)
}

#[test]
fn a_held_switch_leaves_the_badge_alone_and_may_be_ordered_again_then_goes() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    marspot::settings::set_for_test(Settings::default());
    let (mut p, asks) = plugin(&home);
    let host = Host::default();
    p.stop_paying(&host, credit(NOW), bound);
    assert_eq!(asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }), "the menu switch's own check");
    moving::post(SID, CODEX, Verdict::Held(Held::ChildRunning));
    p.follow_switches(&host);
    assert_eq!(p.hint_for(SID), None, "no badge hint for a move nobody asked for");
    assert!(host.logs.lock().unwrap().iter().any(|(t, m)| t == "quota.stop_paying.held" && m == "sid=487 from=P2 why=sub_agent"));
    p.stop_paying(&host, credit(NOW + 60), bound);
    assert_eq!(asks.next(), None, "not asked again within RETRY_S");
    p.stop_paying(&host, credit(NOW + RETRY_S), bound);
    assert_eq!(asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }), "ordered again");
    let old = home.path().join(".codex-profile-2");
    moving::post(SID, CODEX, Verdict::Go { thread: THREAD.into(), home: old, released: Released(pty_op::Job::new()) });
    p.follow_switches(&host);
    assert_eq!(*host.submitted.lock().unwrap(), ["codex.profile_switch"]);
    let ordered = host.logs.lock().unwrap().iter().filter(|(t, _)| t == "quota.stop_paying").count();
    assert_eq!(ordered, 2);
}

#[test]
fn hold_in_the_settings_file_keeps_the_pane_where_it_is() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    let mut s = Settings::default();
    s.sched.credits = SchedCredits::Hold;
    marspot::settings::set_for_test(s);
    let (mut p, asks) = plugin(&home);
    p.stop_paying(&Host::default(), credit(NOW), bound);
    assert_eq!(asks.next(), None);
    marspot::settings::set_for_test(Settings::default());
}

#[test]
fn a_pane_with_text_in_its_input_box_is_not_switched() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    marspot::settings::set_for_test(Settings::default());
    let (mut p, asks) = plugin(&home);
    p.paying.looks.composer = |_| Some("half a sentence".into());
    let host = Host::default();
    p.stop_paying(&host, credit(NOW), bound);
    assert_eq!(asks.next(), None);
    assert!(host.logs.lock().unwrap().iter().any(|(_, m)| m == "sid=487 from=P2 why=input_not_empty"));
}

/// codex 0.172's approval prompt, at the bottom of its screen.
const ASKING: &str = "• Running make\n\n  Would you like to run the following command?\n\n  $ make\n\n› 1. Yes, proceed (y)\n  2. No (esc)\n";

#[test]
fn a_codex_waiting_on_an_approval_is_not_switched() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    marspot::settings::set_for_test(Settings::default());
    let (mut p, asks) = plugin(&home);
    p.paying.looks.screen = |_| Some(ASKING.into());
    let host = Host::default();
    p.stop_paying(&host, credit(NOW), bound);
    assert_eq!(asks.next(), None);
    assert!(host.logs.lock().unwrap().iter().any(|(_, m)| m == "sid=487 from=P2 why=terminal_busy"));
    assert!(!asks_approval("  Would you like to know more? I asked earlier.\n› "), "no question at the row's end");
}

#[test]
fn a_thread_peerbus_does_not_know_is_held_as_unknown() {
    let _turn = moving::test_turn();
    let (home, _env) = sandbox();
    marspot::settings::set_for_test(Settings::default());
    let (mut p, asks) = plugin(&home);
    publish_threads(HashMap::new());
    let host = Host::default();
    p.stop_paying(&host, credit(NOW), bound);
    assert_eq!(asks.next(), None, "the switch refuses an unknown thread; the rule does not ask");
    assert!(host.logs.lock().unwrap().iter().any(|(t, m)| t == "quota.stop_paying.held" && m == "sid=487 from=P2 why=thread_unknown"));
}
