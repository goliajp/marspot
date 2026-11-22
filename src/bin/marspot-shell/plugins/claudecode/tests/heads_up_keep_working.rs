//! A pane told its account is nearly out keeps working on it until the
//! account refuses, and is told to go on once it has been moved.

use super::*;

fn feed(home: &std::path::Path, tag: &str, p1: (&str, f64)) {
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-11-23T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"{tag}1@example","status":"{}","utilization_5h":null,
                "utilization_7d":{},"reset_5h":null,"reset_7d":4102444800,"tier":"default_claude_max_20x"}},
               {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.2,"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x"}}]}}"#,
            p1.0, p1.1
        ),
    )
    .unwrap();
}

fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

fn rebalance_on() {
    marspot::settings::set_for_test(marspot::settings::Settings {
        cc_rebalance: marspot::settings::Rebalance::On,
        ..Default::default()
    });
}

/// The inbox took the heads-up, as the sending thread records it.
fn delivered(plugin: &ClaudecodePlugin, sid: u64) {
    plugin.heads_up.get(&sid).expect("told").delivered.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The moved pane is asked to go on through claude's own command line,
/// after `--resume`, so the resumed claude submits it itself.
fn assert_told_to_go_on(line: &str, work_ended: bool) {
    let word = super::heads_up::after_warned_move(work_ended);
    let (Some(resume), Some(said)) = (line.find("--resume"), line.find(&word)) else {
        panic!("no resume or no continue word: {line}");
    };
    assert!(resume < said, "{line}");
    assert!(!line.contains(&resume_word()), "never \"stay idle\" to a pane that was working: {line}");
}

#[test]
fn a_told_pane_stays_until_its_account_refuses_and_is_then_moved_and_told_to_go_on() {
    rebalance_on();
    let home = quota_home("kw");
    feed(&home, "kw", ("allowed", 0.99));
    let host = FakeHost::new(home.join("state"));
    let sid = 95u64;
    confirmed("u-95");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-95"), sid);
    // busy, with a background shell of its own
    let mut result = scan_of(sid, CcActivity::Working);
    result.new_vetoes.insert(sid, (true, false));

    sweep(&mut plugin, &host, &mut result);
    delivered(&plugin, sid);
    for _ in 0..3 {
        sweep(&mut plugin, &host, &mut result);
    }
    assert!(host.submitted.lock().unwrap().is_empty(), "working on an account that still answers: not moved");
    assert!(plugin.rebalance_said.get(&sid).is_some_and(|k| k.ends_with("|true")), "the rebalance waits on it");

    feed(&home, "kw", ("rejected", 1.0));
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "refused: moved at once, background work and all");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-2"), "{:?}", sent[0].1);
    assert_told_to_go_on(&sent[0].1, true);
}

/// The failure the user reported: a pane that stopped after the
/// heads-up, moved while the old account still had a little left, was
/// told "stay idle if idle" and stayed idle on the new account.
#[test]
fn a_told_pane_that_stopped_is_told_to_go_on_after_any_move() {
    rebalance_on();
    let home = quota_home("ks");
    feed(&home, "ks", ("allowed", 0.99));
    let host = FakeHost::new(home.join("state"));
    let sid = 96u64;
    confirmed("u-96");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-96"), sid);
    // the resting move waits out an overloaded machine; this one is idle
    plugin.host_load = || Some(0.0);
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::Working));
    delivered(&plugin, sid);
    assert!(host.submitted.lock().unwrap().is_empty());

    // it stopped anyway, and nothing runs under it
    host.set_status(
        sid,
        crate::plugins::PaneStatusView {
            status: marspot::pane_state::PaneStatus::AwaitingUser,
            held: Duration::from_secs(600),
            quiescent: true,
        },
    );
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "moved off the account that is running out");
    let sent = host.sent.lock().unwrap().clone();
    assert_told_to_go_on(&sent[0].1, false);
}

/// The same when the account refuses before any resting move.
#[test]
fn a_told_pane_that_stopped_is_told_to_go_on_when_its_account_refuses() {
    marspot::settings::set_for_test(marspot::settings::Settings {
        cc_rebalance: marspot::settings::Rebalance::Off,
        ..Default::default()
    });
    let home = quota_home("kr");
    feed(&home, "kr", ("allowed", 0.99));
    let host = FakeHost::new(home.join("state"));
    let sid = 98u64;
    confirmed("u-98");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-98"), sid);
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::Working));
    delivered(&plugin, sid);

    feed(&home, "kr", ("rejected", 1.0));
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert_eq!(host.submitted.lock().unwrap().len(), 1);
    let sent = host.sent.lock().unwrap().clone();
    assert_told_to_go_on(&sent[0].1, false);
}

/// A switch the person asks for keeps the heads-up's promise too: the
/// told pane is not told "stay idle if idle", and hears that the
/// background work it had running was ended.
#[test]
fn a_told_pane_switched_by_hand_is_told_to_go_on() {
    marspot::settings::set_for_test(marspot::settings::Settings {
        cc_rebalance: marspot::settings::Rebalance::Off,
        ..Default::default()
    });
    let home = quota_home("kh");
    feed(&home, "kh", ("allowed", 0.99));
    let host = FakeHost::new(home.join("state"));
    let sid = 99u64;
    confirmed("u-99");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-99"), sid);
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::Working));
    delivered(&plugin, sid);
    assert!(host.submitted.lock().unwrap().is_empty());

    plugin.last_model.insert(sid, "opus-5-5".into());
    plugin.last_vetoes.insert(sid, (true, false));
    let tag = crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, 2);
    plugin.on_pane_badge_menu_action(&host, sid, tag);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "the pick itself");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-2"), "{:?}", sent[0].1);
    assert_told_to_go_on(&sent[0].1, true);
}

/// A pick by hand on a pane that was never told is unchanged.
#[test]
fn an_untold_pane_switched_by_hand_hears_the_usual_word() {
    let home = quota_home("ku");
    feed(&home, "ku", ("allowed", 0.5));
    let host = FakeHost::new(home.join("state"));
    let sid = 100u64;
    confirmed("u-100");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-100"), sid);
    plugin.last_model.insert(sid, "opus-5-5".into());
    let tag = crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, 2);
    plugin.on_pane_badge_menu_action(&host, sid, tag);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].1.contains(&resume_word()), "{:?}", sent[0].1);
}
