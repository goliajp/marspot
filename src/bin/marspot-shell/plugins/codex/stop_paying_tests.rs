//! `sched.credits = last`: a pane on an account spending paid credits
//! moves, between turns, to the account with the most plan left.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use super::alloc_rollout::{notes_of, Note};
use super::credits::{feed_balances, Acct, Credit, Wallets};
use super::pane::PaneCodex;
use super::stop_paying::{target, Log, Mover, StopPaying, MIN_LEFT, MOVES_PER_HOUR, RETRY_S};
use super::CodexPlugin;
use crate::plugins::peerbus::moving::{self, Ask, Held, Listener, Released, Verdict};
use crate::plugins::peerbus::{publish_threads, ThreadView};
use crate::plugins::{pty_op, LogLevel, PaneStatusView, PluginError, PluginHost};
use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcId;
use marspot::settings::{SchedCredits, Settings};
use marspot_term::test_env::{EnvGuard, TempDir};

pub(super) const NOW: i64 = 1_796_100_000;
pub(super) const H: i64 = 3600;
pub(super) const LAST: SchedCredits = SchedCredits::Last;

/// Codex 2 as it was on 11-29: week used up, balance falling; Codex 1
/// with 91% of its week left.
pub(super) fn fleet() -> Vec<Acct> {
    vec![acct(1, 0.09, false), acct(2, 1.0, true)]
}

pub(super) fn acct(profile: u8, week_used: f64, falling: bool) -> Acct {
    Acct { profile, week_used, renews: NOW + 45 * H, at: NOW - 60, open: true, balance: Some((NOW - 60, 46434.38)), falling }
}

pub(super) fn ok() -> Result<(), &'static str> {
    Ok(())
}

pub(super) fn mv(sid: u64, from: u8) -> Mover {
    Mover { sid, from, need: MIN_LEFT }
}

pub(super) fn credit(now: i64) -> Option<Credit> {
    Some(Credit { now, accts: fleet(), rates: HashMap::new() })
}

fn tags(log: &Log) -> Vec<&'static str> {
    log.iter().map(|(t, _)| *t).collect()
}

/// The token_count line a codex 0.172 rollout writes, as on 11-28.
fn token_count(balance: &str, used_percent: f64) -> String {
    format!(
        r#"{{"timestamp":"2026-11-28T13:33:06.355Z","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used_percent},"window_minutes":10080,"resets_at":1796309547}},"secondary":null,"credits":{{"has_credits":true,"unlimited":false,"balance":"{balance}"}},"individual_limit":null,"spend_control_reached":null,"plan_type":"pro","rate_limit_reached_type":null}}}}}}"#
    )
}

#[test]
fn a_rollout_line_gives_the_balance_beside_the_week() {
    let notes = notes_of(&token_count("60460.5367250000", 100.0));
    let at = marspot::cc_usage::parse_iso_utc("2026-11-28T13:33:06.355Z".into()).unwrap();
    assert_eq!(notes, [Note::Limits { at, week: Some((1.0, 1796309547)), balance: Some(60460.536725), cap: None, tokens: None }]);
}

#[test]
fn the_feed_gives_each_profiles_balance() {
    let body = r#"{"generated_at":"2026-11-29T03:15:13+00:00","accounts":[
      {"name":"Codex 1","status":"allowed","utilization_7d":0.09,"credits":{"enabled":true,"unlimited":false,"balance":60460.536725}},
      {"name":"Codex 2","status":"rejected","utilization_7d":1.0,"credits":{"enabled":true,"balance":"46433.218085"}},
      {"name":"Claude 3","credits":{"balance":5}}]}"#;
    let (at, list) = feed_balances(body).expect("a feed");
    assert_eq!(at, marspot::cc_usage::parse_iso_utc("2026-11-29T03:15:13+00:00".into()).unwrap());
    assert_eq!(list, [(1, 60460.536725), (2, 46433.218085)]);
    assert_eq!(feed_balances("{not json"), None);
}

#[test]
fn a_balance_falls_only_while_the_week_is_used_up() {
    let week = NOW + 45 * H;
    let mut w = Wallets::default();
    w.saw(2, NOW - 300, 59775.63, None);
    w.saw(2, NOW - 200, 59775.63, Some(week));
    assert!(!w.fell(2, week), "flat is not paying");
    w.saw(2, NOW - 100, 59773.90, None);
    assert!(!w.fell(2, week), "a fall with the week not used up is not paying");
    w.saw(2, NOW - 80, 59700.00, Some(week));
    assert!(!w.fell(2, week), "a fall from a reading before the week was used up is not paying");
    w.saw(2, NOW - 50, 59690.00, Some(week + 7 * 86_400));
    assert!(!w.fell(2, week + 7 * 86_400), "a fall across a renewal is not paying");
    w.saw(2, NOW - 40, 59690.00, Some(week));
    w.saw(2, NOW - 30, 59618.80, Some(week));
    assert!(w.fell(2, week), "fell between two readings of the same used-up week");
    w.saw(2, NOW - 400, 99999.0, Some(week));
    assert_eq!(w.balance(2), Some((NOW - 30, 59618.80)), "an older reading is history");
    assert!(!w.fell(2, week + 7 * 86_400), "next week starts clean");
    assert!(!w.fell(1, week), "another account is its own");
}

#[test]
fn paying_needs_both_a_used_up_week_and_a_falling_balance() {
    assert!(acct(2, 1.0, true).paying(NOW));
    assert!(!acct(2, 1.0, false).paying(NOW), "flat balance");
    assert!(!acct(2, 0.98, true).paying(NOW), "week under 100%");
    let mut renewed = acct(2, 1.0, true);
    renewed.renews = NOW - 1;
    assert!(!renewed.paying(NOW), "the week renewed");
    let mut refused = acct(2, 0.97, true);
    refused.open = false;
    assert!(refused.paying(NOW), "the feed says it refuses and the balance falls");
}

#[test]
fn the_target_has_the_most_plan_left_and_is_neither_paying_nor_stale() {
    let mut accts = vec![acct(1, 0.40, false), acct(2, 1.0, true), acct(3, 0.10, false), acct(4, 0.10, false)];
    assert_eq!(target(&accts, 2, NOW, MIN_LEFT).map(|a| a.profile), Some(3), "most left, the lower of a tie");
    accts[2].at = NOW - 31 * 60;
    assert_eq!(target(&accts, 2, NOW, MIN_LEFT).map(|a| a.profile), Some(4), "a stale reading is no target");
    accts[3].open = false;
    assert_eq!(target(&accts, 2, NOW, MIN_LEFT).map(|a| a.profile), Some(1), "a refusing account is no target");
    accts[0].week_used = 1.0;
    assert_eq!(target(&accts, 2, NOW, MIN_LEFT), None, "used up is no target");
    assert_eq!(target(&[acct(1, 1.0, true), acct(2, 1.0, true)], 2, NOW, MIN_LEFT), None, "a paying account is no target");
}

#[test]
fn a_paying_pane_moves_once_with_the_log_line() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    let o = s.decide(LAST, &fleet(), NOW, mv(487, 2), ok, &mut log).expect("ordered");
    assert_eq!((o.from, o.to), (2, 1));
    assert_eq!(log, [("quota.stop_paying", "sid=487 from=P2 to=P1 balance_before=46434.38 plan_left_to=0.91 reason=paying".to_string())]);
    log.clear();
    assert_eq!(s.decide(LAST, &fleet(), NOW + 60, mv(487, 2), ok, &mut log), None, "once per used-up week");
    assert_eq!(log, [("quota.stop_paying.held", "sid=487 from=P2 why=moved_once".to_string())]);
    let mut next_week = fleet();
    next_week[1].renews += 7 * 86_400;
    assert!(s.decide(LAST, &next_week, NOW + 120, mv(487, 2), ok, &mut log).is_some(), "the next used-up week may move it again");
}

#[test]
fn nothing_moves_that_is_not_paying_or_has_nowhere_to_go() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    assert_eq!(s.decide(LAST, &fleet(), NOW, mv(5, 1), || panic!("not asked"), &mut log), None, "P1 is not paying");
    let mut broke = vec![acct(1, 0.09, false), acct(2, 1.0, false)];
    broke[1].balance = Some((NOW - 60, 0.0));
    assert_eq!(s.decide(LAST, &broke, NOW, mv(487, 2), || panic!("not asked"), &mut log), None, "no credits to spend");
    broke[1].balance = None;
    assert_eq!(s.decide(LAST, &broke, NOW, mv(487, 2), || panic!("not asked"), &mut log), None, "no balance known");
    assert!(log.is_empty(), "{log:?}");
    let all_out = vec![acct(1, 1.0, false), acct(2, 1.0, true)];
    assert_eq!(s.decide(LAST, &all_out, NOW, mv(487, 2), || panic!("not asked"), &mut log), None);
    assert_eq!(log, [("quota.stop_paying.held", "sid=487 from=P2 why=no_target".to_string())]);
}

#[test]
fn hold_and_free_move_nothing() {
    for mode in [SchedCredits::Hold, SchedCredits::Free] {
        let mut s = StopPaying::default();
        let mut log = Log::new();
        assert_eq!(s.decide(mode, &fleet(), NOW, mv(487, 2), || panic!("not asked"), &mut log), None, "{mode:?}");
        assert!(log.is_empty(), "{mode:?}: {log:?}");
    }
}

#[test]
fn a_pane_not_ready_is_held_and_said_once_per_reason() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    for why in ["busy", "busy", "input_not_empty", "input_not_empty", "cool_down"] {
        assert_eq!(s.decide(LAST, &fleet(), NOW, mv(487, 2), || Err(why), &mut log), None);
    }
    let said: Vec<&str> = log.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(said, ["sid=487 from=P2 why=busy", "sid=487 from=P2 why=input_not_empty", "sid=487 from=P2 why=cool_down"]);
    log.clear();
    assert!(s.decide(LAST, &fleet(), NOW, mv(487, 2), ok, &mut log).is_some(), "ready at last");
}

#[test]
fn the_fleet_moves_at_most_four_an_hour() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    for sid in 0..MOVES_PER_HOUR as u64 {
        assert!(s.decide(LAST, &fleet(), NOW, mv(sid, 2), ok, &mut log).is_some());
    }
    log.clear();
    assert_eq!(s.decide(LAST, &fleet(), NOW + 60, mv(99, 2), ok, &mut log), None);
    assert_eq!(tags(&log), ["quota.stop_paying.held"]);
    assert!(log[0].1.ends_with("why=rate"));
    let fresh: Vec<Acct> = fleet().into_iter().map(|a| Acct { at: a.at + H, ..a }).collect();
    assert!(s.decide(LAST, &fresh, NOW + H, mv(99, 2), ok, &mut log).is_some(), "an hour on, with fresh readings");
}

#[test]
fn the_old_accounts_next_balance_is_logged_once() {
    let mut s = StopPaying::default();
    let mut log = Log::new();
    s.decide(LAST, &fleet(), NOW, mv(487, 2), ok, &mut log).expect("ordered");
    log.clear();
    s.follow_afters(&fleet(), NOW + 60, &mut log);
    assert!(log.is_empty(), "no reading since the order");
    let mut later = fleet();
    later[1].balance = Some((NOW + 300, 46420.0));
    s.follow_afters(&later, NOW + 300, &mut log);
    assert_eq!(log, [("quota.stop_paying.after", format!("sid=487 from=P2 balance_before=46434.38 balance_after=46420.00 read_at={} fell_by=14.38", NOW + 300))]);
    log.clear();
    s.follow_afters(&later, NOW + 400, &mut log);
    assert!(log.is_empty(), "once");
}

#[derive(Default)]
pub(super) struct Host {
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

pub(super) const SID: u64 = 487;
pub(super) const CODEX: ProcId = ProcId { pid: i32::MAX, start_us: 1 };
pub(super) const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

/// A codex pane on profile 2, bound idle to its thread, with an empty
/// input box, and a worker that takes asks without answering them.
pub(super) fn plugin(home: &TempDir) -> (CodexPlugin, Listener) {
    let mut p = CodexPlugin::new();
    p.paying.looks.screen = |_| None;
    p.paying.looks.composer = |_| Some(String::new());
    p.panes.insert(SID, PaneCodex { codex: CODEX, shell_pid: i32::MAX, profile: Some(2), argv: Some(vec!["codex".into()]) });
    let home2 = home.path().join(".codex-profile-2");
    publish_threads(HashMap::from([(SID, ThreadView { codex: CODEX, home: home2, thread: THREAD.into(), idle: true, ends: Default::default() })]));
    (p, Listener::connect())
}

pub(super) fn sandbox() -> (TempDir, EnvGuard) {
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
    p.stop_paying(&host, credit(NOW));
    assert_eq!(asks.next(), Some(Ask::Begin { sid: SID, codex: CODEX }), "the menu switch's own check");
    moving::post(SID, CODEX, Verdict::Held(Held::ChildRunning));
    p.follow_switches(&host);
    assert_eq!(p.hint_for(SID), None, "no badge hint for a move nobody asked for");
    assert!(host.logs.lock().unwrap().iter().any(|(t, m)| t == "quota.stop_paying.held" && m == "sid=487 from=P2 why=sub_agent"));
    p.stop_paying(&host, credit(NOW + 60));
    assert_eq!(asks.next(), None, "not asked again within RETRY_S");
    p.stop_paying(&host, credit(NOW + RETRY_S));
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
    p.stop_paying(&Host::default(), credit(NOW));
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
    p.stop_paying(&host, credit(NOW));
    assert_eq!(asks.next(), None);
    assert!(host.logs.lock().unwrap().iter().any(|(_, m)| m == "sid=487 from=P2 why=input_not_empty"));
}
