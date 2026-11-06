//! Telling a busy pane its account is nearly out, and what that buys
//! it when the account does run out.

use super::*;

fn feed(home: &std::path::Path, tag: &str, p1: (&str, f64), p2: (&str, f64)) {
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-11-06T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"{tag}1@example","status":"{}","utilization_5h":null,
                "utilization_7d":{},"reset_5h":null,"reset_7d":4102444800,"tier":"default_claude_max_20x"}},
               {{"name":"B","email":"{tag}2@example","status":"{}","utilization_5h":null,
                "utilization_7d":{},"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x"}}]}}"#,
            p1.0, p1.1, p2.0, p2.1
        ),
    )
    .unwrap();
}

/// The hook's record of the model, without which a pane takes no part.
fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

fn resting(host: &FakeHost, sid: u64) {
    host.status.lock().unwrap().insert(
        sid,
        crate::plugins::PaneStatusView {
            status: marspot::pane_state::PaneStatus::AwaitingUser,
            held: Duration::from_secs(600),
            quiescent: true,
        },
    );
}

fn off() {
    marspot::settings::set_for_test(marspot::settings::Settings {
        cc_rebalance: marspot::settings::Rebalance::Off,
        ..Default::default()
    });
}

#[test]
fn a_busy_pane_is_told_once_when_its_account_is_one_step_from_out() {
    off();
    let home = quota_home("hu");
    feed(&home, "hu", ("allowed", 0.99), ("allowed", 0.2));
    let host = FakeHost::new(home.join("state"));
    let sid = 90u64;
    confirmed("u-90");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-90"), sid);
    let mut result = scan_of(sid, CcActivity::Working);

    sweep(&mut plugin, &host, &mut result);
    let told = plugin.heads_up.get(&sid).expect("told");
    assert_eq!((told.profile, told.week), (1, 4102444800));
    let first = Arc::as_ptr(&told.delivered);
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(Arc::as_ptr(&plugin.heads_up[&sid].delivered), first, "once, not every pass");
    assert!(host.submitted.lock().unwrap().is_empty(), "telling it moves nothing");
}

#[test]
fn a_resting_pane_or_one_with_nowhere_to_go_is_not_told() {
    off();
    let home = quota_home("hr");
    feed(&home, "hr", ("allowed", 0.99), ("allowed", 0.2));
    let host = FakeHost::new(home.join("state"));
    let sid = 91u64;
    confirmed("u-91");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-91"), sid);
    resting(&host, sid);
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.is_empty(), "moving a resting pane ends nothing");

    // busy, but every other account is out: there is no move to warn of
    feed(&home, "hr", ("allowed", 0.99), ("rejected", 1.0));
    let mut result = scan_of(sid, CcActivity::Working);
    host.status.lock().unwrap().clear();
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.is_empty());

    // and an account with more than the last step left is not news
    feed(&home, "hr", ("allowed", 0.98), ("allowed", 0.2));
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.is_empty());
}

#[test]
fn a_pane_told_in_time_leaves_at_once_and_hears_its_background_work_ended() {
    off();
    let home = quota_home("ht");
    feed(&home, "ht", ("rejected", 1.0), ("allowed", 0.2));
    let host = FakeHost::new(home.join("state"));
    let sid = 92u64;
    confirmed("u-92");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-92"), sid);
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    result.new_vetoes.insert(sid, (true, false));

    // not told: the background work holds it, as before
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty(), "held for its background work");

    // told, and the inbox took it: it goes now
    plugin.heads_up.insert(
        sid,
        super::heads_up::Sent {
            profile: 1,
            week: 4102444800,
            delivered: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        },
    );
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "moved without the wait");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-2"), "{:?}", sent[0].1);
    assert!(sent[0].1.contains(&resume_word()), "{:?}", sent[0].1);
    assert!(sent[0].1.contains("后台任务"), "told its background work was ended: {:?}", sent[0].1);
}

#[test]
fn a_move_with_nothing_running_says_only_the_usual_sentence() {
    off();
    let home = quota_home("hn");
    feed(&home, "hn", ("rejected", 1.0), ("allowed", 0.2));
    let host = FakeHost::new(home.join("state"));
    let sid = 93u64;
    confirmed("u-93");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-93"), sid);
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    sweep(&mut plugin, &host, &mut result);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.contains(&resume_word()));
    assert!(!sent[0].1.contains("后台任务"), "{:?}", sent[0].1);
}

#[test]
fn who_was_told_is_still_known_after_the_shell_restarts() {
    off();
    let home = quota_home("hp");
    feed(&home, "hp", ("allowed", 0.99), ("allowed", 0.2));
    let host = FakeHost::new(home.join("state"));
    let sid = 94u64;
    confirmed("u-94");
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-94"), sid);
    let mut result = scan_of(sid, CcActivity::Working);
    sweep(&mut plugin, &host, &mut result);
    assert!(plugin.heads_up.contains_key(&sid));

    // a new process, as after a silent update: read back, not told again
    let mut again = stranded_plugin(pane_on_p1(&home, "u-94"), sid);
    again.load_heads_up(&host);
    assert!(again.heads_up.get(&sid).is_some_and(|h| h.is_for(1, 4102444800)));
}
