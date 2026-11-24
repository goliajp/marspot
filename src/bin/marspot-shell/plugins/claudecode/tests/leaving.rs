//! Leaving an account that cannot last two hours: the resting panes on
//! it go, the slowest stays, and every one working on it is told.

use super::*;
use marspot::cc_usage::{CcAccount, CcModelLimit, CcUsage};

pub(super) const RENEWS: i64 = 4102444800;

/// One account of the feed, as JSON, collected `at`.
pub(super) fn account(mail: &str, u7: f64, reset: i64, at: i64) -> String {
    format!(
        r#"{{"name":"{mail}","email":"{mail}","status":"allowed","utilization_5h":null,
            "utilization_7d":{u7},"reset_5h":null,"reset_7d":{reset},"tier":"default_claude_max_20x",
            "collected_at":"{}","model_limits":[{{"label":"Fable","utilization":0.1,"reset":{reset}}}]}}"#,
        crate::plugins::usage::iso_utc(at)
    )
}

pub(super) fn write_feed(home: &std::path::Path, accounts: &[String]) {
    let body = format!(r#"{{"generated_at": "2026-11-25T00:00:00+00:00", "accounts": [{}]}}"#, accounts.join(","));
    fs::write(home.join(".local/state/devops/claude-usage.json"), body).unwrap();
}

/// The feed as the sweep reads it now: P1 at `u1` of its week, P2 at
/// a fifth.
fn feed(home: &std::path::Path, tag: &str, u1: f64) {
    let now = rooms::unix_now();
    write_feed(home, &[account(&format!("{tag}1@example"), u1, RENEWS, now), account(&format!("{tag}2@example"), 0.2, RENEWS + 100, now)]);
}

/// What the scan thread measures: an hour of readings before the feed
/// on disk, each account in `rising` going up `per_hour` to where the
/// feed has it now. The feed's own reading is the newest.
pub(super) fn measured(result: &mut ScanResult, rising: &[(&str, f64)]) {
    let now = rooms::unix_now();
    let feed = marspot::cc_usage::read().expect("the feed");
    let mut cache = super::burn::TrendCache::default();
    let mut log = Vec::new();
    for k in (1..=12i64).rev() {
        let at = now - k * 300;
        let accounts: Vec<CcAccount> = feed
            .accounts
            .iter()
            .filter_map(|a| {
                let (_, speed) = rising.iter().find(|(m, _)| *m == a.email)?;
                let u = a.util_7d? - speed * k as f64 / 12.0;
                let fable = vec![CcModelLimit { label: "Fable".into(), util: Some(0.1), reset: a.reset_7d }];
                Some(CcAccount { util_7d: Some(u), collected_at: at, model_limits: fable, ..a.clone() })
            })
            .collect();
        cache.take(Some(&CcUsage { generated_at: at, accounts }), at, &mut log);
    }
    assert!(feed.accounts.iter().all(|a| a.collected_at > now - 60), "the feed is current");
    result.speeds = cache.take(Some(&feed), now, &mut log);
}

/// P1 rising six points an hour.
fn p1_trend(result: &mut ScanResult, tag: &str) {
    measured(result, &[(&format!("{tag}1@example"), 0.06)]);
}

pub(super) fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

pub(super) fn rest(host: &FakeHost, sid: u64) {
    host.set_status(sid, crate::plugins::PaneStatusView {
        status: marspot::pane_state::PaneStatus::AwaitingUser,
        held: Duration::from_secs(600),
        quiescent: true,
    });
}

pub(super) fn rebalance_mode(mode: marspot::settings::Rebalance) {
    marspot::settings::set_for_test(marspot::settings::Settings { cc_rebalance: mode, ..Default::default() });
}

fn dryrun() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
}

/// Panes on `profile` with these sessions and token weights, all
/// resting unless `busy`.
pub(super) fn panes_on(home: &TestHome, tag: &str, host: &FakeHost, profile: u8, panes: &[(u64, f64, bool)]) -> (ClaudecodePlugin, ScanResult) {
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.host_load = || None;
    let mut result = scan_of(panes[0].0, CcActivity::AwaitingUser);
    result.sessions_seen.clear();
    for &(sid, tokens, busy) in panes {
        let uuid = format!("u-{tag}-{sid}");
        confirmed(&uuid);
        let mut meta = pane_on_p1(home, &uuid);
        meta.profile_num = profile;
        meta.config_dir = Some(home.join(format!(".claude-profile-{profile}")).to_string_lossy().into_owned());
        plugin.last_meta.insert(sid, meta);
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

fn panes_on_p1(home: &TestHome, tag: &str, host: &FakeHost, panes: &[(u64, f64, bool)]) -> (ClaudecodePlugin, ScanResult) {
    panes_on(home, tag, host, 1, panes)
}

pub(super) fn moved(host: &FakeHost) -> Vec<u64> {
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
    p1_trend(&mut result, "lv");
    plugin.rebalanced.insert(203, (Instant::now() - Duration::from_secs(600), 1));

    sweep(&mut plugin, &host, &mut result);
    assert_eq!(moved(&host), vec![201, 203], "the two faster panes leave in one pass");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent.iter().all(|(_, s)| s.contains(".claude-profile-2")), "{sent:?}");
    let logged = host.logged.lock().unwrap().clone();
    assert!(logged.iter().any(|(e, l)| e == "quota.short" && l.contains("P1 lasts 1.5h") && l.contains("sid=202 stays")), "{logged:?}");
    assert_eq!(logged.iter().filter(|(e, _)| e == "quota.leaving").count(), 2, "{logged:?}");
    assert!(!logged.iter().any(|(e, _)| e.starts_with("rebalance.")), "not an ordinary move: {logged:?}");
    // each move is a fact with the inputs that ranked it and the
    // accounts it was ranked among
    let dir = super::super::allocation_adapter::events_dir(&marspot::paths::state_root());
    let text: String = fs::read_dir(&dir).unwrap().map(|e| fs::read_to_string(e.unwrap().path()).unwrap()).collect();
    let details: Vec<&str> = text.lines().filter(|l| l.split('\t').nth(3) == Some("moved")).filter_map(|l| l.split('\t').nth(6)).collect();
    assert_eq!(details.len(), 2, "{text}");
    for d in details {
        assert!(d.starts_with("from=P1 why=leave_early among=P2 here=1 "), "{d}");
        assert!(crate::plugins::allocation::replay::replay(d).is_some(), "the inputs read back: {d}");
    }
}

/// Ordinary moves switched off do not hold it either.
#[test]
fn rebalance_off_does_not_hold_leaving_a_short_account() {
    rebalance_mode(marspot::settings::Rebalance::Off);
    let home = quota_home("lvoff");
    feed(&home, "lvoff", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "lvoff", &host, &[(241, 900.0, false), (242, 100.0, false)]);
    p1_trend(&mut result, "lvoff");
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(moved(&host), vec![241]);
}

#[test]
fn rebalance_the_last_pane_on_a_short_account_does_not_leave() {
    dryrun();
    let home = quota_home("lone");
    feed(&home, "lone", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "lone", &host, &[(211, 900.0, false)]);
    p1_trend(&mut result, "lone");
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
    p1_trend(&mut result, "long");
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
    p1_trend(&mut result, "lvh");
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

/// Back over two hours is not yet recovered: the busy panes are not
/// told again and the account is not logged as over it until it is
/// seen to last four.
#[test]
fn heads_up_and_the_short_line_wait_for_four_hours_before_counting_an_account_recovered() {
    dryrun();
    let home = quota_home("rec");
    feed(&home, "rec", 0.91);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on_p1(&home, "rec", &host, &[(251, 900.0, true), (252, 100.0, true)]);
    let p1 = "rec1@example";
    measured(&mut result, &[(p1, 0.06)]);
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.contains_key(&251));
    let over = |host: &FakeHost| host.logged.lock().unwrap().iter().filter(|(e, _)| e == "quota.short_over").count();

    // nine points at three an hour: three hours
    measured(&mut result, &[(p1, 0.03)]);
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.contains_key(&251), "three hours is not a recovery");
    assert_eq!(over(&host), 0);

    // at 1.8 an hour: five
    measured(&mut result, &[(p1, 0.018)]);
    sweep(&mut plugin, &host, &mut result);
    assert!(!plugin.heads_up.contains_key(&251), "recovered: told again next time");
    assert_eq!(over(&host), 1, "{:?}", host.logged.lock().unwrap());
}
