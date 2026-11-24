//! Leaving an account that cannot last two hours: the resting panes on
//! it go, the slowest stays, and every one working on it is told.

use super::*;
use marspot::cc_usage::{CcAccount, CcModelLimit};

const RENEWS: i64 = 4102444800;

/// The feed as the sweep reads it now: P1 at `u1` of its week, P2 at
/// a fifth.
fn feed(home: &std::path::Path, tag: &str, u1: f64) {
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-11-25T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"{tag}1@example","status":"allowed","utilization_5h":null,
                "utilization_7d":{u1},"reset_5h":null,"reset_7d":{RENEWS},"tier":"default_claude_max_20x",
                "model_limits":[{{"label":"Fable","utilization":0.1,"reset":{RENEWS}}}]}},
               {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.2,"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x",
                "model_limits":[{{"label":"Fable","utilization":0.1,"reset":4102444900}}]}}]}}"#
        ),
    )
    .unwrap();
}

/// An hour of P1's readings, its week rising six points an hour to `u1`.
fn p1_trend(plugin: &mut ClaudecodePlugin, tag: &str, u1: f64) {
    let at0 = 1_795_000_000;
    for k in 0..=12i64 {
        let u = u1 - 0.06 * (12 - k) as f64 / 12.0;
        plugin.leave.trends.note(&[CcAccount {
            name: "A".into(),
            email: format!("{tag}1@example"),
            status: "allowed".into(),
            util_5h: None,
            util_7d: Some(u),
            reset_5h: None,
            reset_7d: Some(RENEWS),
            model_limits: vec![CcModelLimit { label: "Fable".into(), util: Some(0.1), reset: Some(RENEWS) }],
            collected_at: at0 + k * 300,
            limits_as_of: None,
            tier: "default_claude_max_20x".into(),
        }]);
    }
}

fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

fn rest(host: &FakeHost, sid: u64) {
    host.set_status(sid, crate::plugins::PaneStatusView {
        status: marspot::pane_state::PaneStatus::AwaitingUser,
        held: Duration::from_secs(600),
        quiescent: true,
    });
}

fn dryrun() {
    marspot::settings::set_for_test(marspot::settings::Settings {
        cc_rebalance: marspot::settings::Rebalance::DryRun,
        ..Default::default()
    });
}

/// Panes on P1 with these sessions and token weights, all resting
/// unless `busy`.
fn panes_on_p1(home: &TestHome, tag: &str, host: &FakeHost, panes: &[(u64, f64, bool)]) -> (ClaudecodePlugin, ScanResult) {
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.host_load = || None;
    let mut result = scan_of(panes[0].0, CcActivity::AwaitingUser);
    result.sessions_seen.clear();
    for &(sid, tokens, busy) in panes {
        let uuid = format!("u-{tag}-{sid}");
        confirmed(&uuid);
        plugin.last_meta.insert(sid, pane_on_p1(home, &uuid));
        result.sessions_seen.push(sid);
        result.new_models.insert(sid, "opus-5-5".into());
        result.new_vetoes.insert(sid, (false, false));
        result.new_activity.insert(sid, if busy { CcActivity::Working } else { CcActivity::AwaitingUser });
        result.pane_tokens.insert(sid, tokens);
        if !busy {
            rest(host, sid);
        }
    }
    (plugin, result)
}

fn moved(host: &FakeHost) -> Vec<u64> {
    let mut sids: Vec<u64> = host.submitted.lock().unwrap().iter().map(|(sid, _)| *sid).collect();
    sids.sort();
    sids
}

/// Six points an hour with nine left is an hour and a half. The
/// ordinary moves are set to a dry run, and one pane was moved ten
/// minutes ago: neither holds this move.
#[test]
fn rebalance_resting_panes_leave_an_account_with_an_hour_and_a_half_left_but_the_slowest_stays() {
    dryrun();
    let home = quota_home("lv");
    feed(&home, "lv", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "lv", &host, &[(201, 900.0, false), (202, 100.0, false), (203, 500.0, false)]);
    p1_trend(&mut plugin, "lv", 0.91);
    plugin.rebalanced.insert(203, (Instant::now() - Duration::from_secs(600), 1));

    sweep(&mut plugin, &host, &mut result);
    assert_eq!(moved(&host), vec![201, 203], "the two faster panes leave in one pass");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent.iter().all(|(_, s)| s.contains(".claude-profile-2")), "{sent:?}");
    let logged = host.logged.lock().unwrap().clone();
    assert!(logged.iter().any(|(e, l)| e == "quota.short" && l.contains("sid=202 stays")), "{logged:?}");
    assert_eq!(logged.iter().filter(|(e, _)| e == "quota.leaving").count(), 2, "{logged:?}");
    assert!(!logged.iter().any(|(e, _)| e.starts_with("rebalance.")), "not an ordinary move: {logged:?}");
}

#[test]
fn rebalance_the_last_pane_on_a_short_account_does_not_leave() {
    dryrun();
    let home = quota_home("lone");
    feed(&home, "lone", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "lone", &host, &[(211, 900.0, false)]);
    p1_trend(&mut plugin, "lone", 0.91);
    sweep(&mut plugin, &host, &mut result);
    assert!(moved(&host).is_empty(), "it uses what is left");
}

/// Twenty points at six an hour lasts over three hours.
#[test]
fn rebalance_an_account_lasting_over_two_hours_is_not_left() {
    dryrun();
    let home = quota_home("long");
    feed(&home, "long", 0.80);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "long", &host, &[(221, 900.0, false), (222, 100.0, true)]);
    p1_trend(&mut plugin, "long", 0.80);
    sweep(&mut plugin, &host, &mut result);
    assert!(moved(&host).is_empty());
    assert!(plugin.heads_up.is_empty(), "nobody told");
}

/// The heads-up has the same trigger, and the pane that stays hears it
/// as well as the one that will leave once it rests.
#[test]
fn heads_up_reaches_every_busy_pane_on_a_short_account_including_the_one_that_stays() {
    dryrun();
    let home = quota_home("lvh");
    feed(&home, "lvh", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "lvh", &host, &[(231, 900.0, true), (232, 100.0, true)]);
    p1_trend(&mut plugin, "lvh", 0.91);
    sweep(&mut plugin, &host, &mut result);
    assert!(moved(&host).is_empty(), "busy panes are not moved");
    for sid in [231, 232] {
        assert!(plugin.heads_up.get(&sid).is_some_and(|h| h.is_for(1, RENEWS)), "sid {sid} told");
    }
    let part = super::leave_early::Part { profile: 1, short: true, stays: true, speed: 0.0, hours: Some(1.5) };
    let body = super::leave_early::heads_up_body(1, part);
    assert!(body.contains("撑不到 2 小时") && body.contains("1.5"), "{body}");

    // the faster one comes to rest: it leaves, told its switch is done
    rest(&host, 231);
    result.new_activity.insert(231, CcActivity::AwaitingUser);
    plugin.heads_up.get(&231).unwrap().delivered.store(true, std::sync::atomic::Ordering::Relaxed);
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(moved(&host), vec![231]);
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(&super::heads_up::after_warned_move(false)), "{:?}", sent[0].1);
}
