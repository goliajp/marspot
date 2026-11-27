//! A session refused Fable is moved to an account with Fable left and
//! told what to start again, and is not moved off such an account to
//! leave Fable to others.

use super::super::fable_need::Ask;
use super::*;

/// The notice the CLI queued on 2026-11-22 19:03:43 UTC when a
/// background agent of an Opus session on P6 was refused Fable,
/// trimmed to the fields read.
const AGENT_REFUSED_FABLE: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-22T19:03:43.402Z","sessionId":"153c990a","content":"<task-notification>\n<task-id>abe43e308aaebff57</task-id>\n<status>failed</status>\n<summary>Agent \"Design RFC: agent control, shadow subpanes\" failed: Agent terminated early due to an API error: You've hit your Fable limit · resets Nov 27 at 2pm (Asia/Tokyo) (error type rate_limit, HTTP 429, request id req_011ChJVA, model sent to the API: claude-fable-5-2)</summary>\n</task-notification>"}"#;

/// The session's own request refused, as written on 2026-11-14
/// (trimmed).
const SESSION_REFUSED_FABLE: &str = concat!(
    r#"{"parentUuid":"p","isSidechain":false,"type":"assistant","timestamp":"2026-11-14T00:50:01.266Z","#,
    r#""message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","#,
    r#""text":"You've hit your Fable limit · resets Nov 20 at 2pm (Asia/Tokyo)"}]},"#,
    r#""apiError":"usage_limit_reached","error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#,
);

#[test]
fn a_fable_refusal_is_read_from_the_agent_notice_and_from_the_session_itself() {
    let mut ctx = ctx_for_test(PathBuf::from("/nonexistent"));
    let long_ago = std::time::UNIX_EPOCH;
    let told = ctx.told(None, Some(AGENT_REFUSED_FABLE), long_ago);
    assert!(told.own.is_none());
    assert_eq!(
        told.fable,
        Some(Ask { at: 1_795_374_223, agent: Some("Design RFC: agent control, shadow subpanes".into()) })
    );

    // the session's own refusal is remembered per transcript
    let told = ctx.told(Some(&PathBuf::from("/t/x.jsonl")), Some(SESSION_REFUSED_FABLE), long_ago);
    assert_eq!(told.fable, Some(Ask { at: 1_794_617_401, agent: None }));

    // refused under an earlier claude: not this one's
    let started = std::time::UNIX_EPOCH + Duration::from_secs(1_795_400_000);
    assert_eq!(ctx.told(None, Some(AGENT_REFUSED_FABLE), started).fable, None);

    // an Opus refusal is not about Fable
    let opus = AGENT_REFUSED_FABLE.replace("Fable limit", "Opus limit").replace("claude-fable-5-2", "claude-opus-5-5");
    let told = ctx.told(None, Some(&opus), long_ago);
    assert!(told.agent.is_some() && told.fable.is_none(), "{:?}", told.agent);
}

fn now() -> i64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Two accounts with room for Opus, both measured on 2026-11-20. P1's
/// week renews first.
fn feed(home: &std::path::Path, tag: &str, fable_p1: f64, fable_p2: f64) {
    fs::write(
        usage_feed(home),
        format!(
            r#"{{"generated_at": "2026-11-20T00:00:00+00:00", "accounts": [
               {{"name":"A","email":"{tag}1@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.3,"reset_5h":null,"reset_7d":4102444800,"tier":"default_claude_max_20x",
                "collected_at":"2026-11-20T00:00:00+00:00",
                "model_limits":[{{"label":"Fable","utilization":{fable_p1},"reset":4102444800}}]}},
               {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.3,"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x",
                "collected_at":"2026-11-20T00:00:00+00:00",
                "model_limits":[{{"label":"Fable","utilization":{fable_p2},"reset":4102444900}}]}}]}}"#
        ),
    )
    .unwrap();
}

fn confirmed(uuid: &str) {
    fs::write(model_push_dir().join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

fn rebalance(mode: marspot::settings::Rebalance) {
    marspot::settings::set_for_test(marspot::settings::Settings { cc_rebalance: mode, ..Default::default() });
}

/// A background agent of the Opus pane `sid` refused Fable just now.
fn agent_refused_fable(sid: u64) -> ScanResult {
    let at = now();
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    result.agent_refused.insert(
        sid,
        quota::Refusal { at, model: "fable-5-2".into(), logged_out: false, window: quota::Window::Week },
    );
    result.fable_refused.insert(sid, Ask { at, agent: Some("Design RFC".into()) });
    result
}

fn lines_about(host: &FakeHost, sid: u64) -> Vec<(String, String)> {
    let tag = format!("sid={sid} ");
    host.logged.lock().unwrap().iter().filter(|(_, l)| l.contains(&tag)).cloned().collect()
}

/// The feed still reads P1's Fable at half: it runs behind. The
/// refusal is what says P1 has none left, and the pane goes at once.
#[test]
fn an_opus_session_refused_fable_moves_to_an_account_with_fable_and_is_told_to_start_again() {
    let home = quota_home("fable-move");
    feed(&home, "fable-move", 0.5, 0.1);
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-82");
    let host = FakeHost::new(home.join("state"));
    let sid = 82u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-82"), sid);

    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert!(host.submitted.lock().unwrap().is_empty(), "an Opus pane on P1 has room");

    sweep(&mut plugin, &host, &mut agent_refused_fable(sid));
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "moved on the pass that read the refusal");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-2"), "to P2, which has Fable: {:?}", sent[0].1);
    assert!(sent[0].1.contains("--model 'claude-opus-5-5'"), "on the model it was on: {:?}", sent[0].1);
    assert!(sent[0].1.contains("Design RFC") && sent[0].1.contains("P2"), "{:?}", sent[0].1);
    assert!(sent[0].1.contains("重新启动这个子代理"), "{:?}", sent[0].1);
    assert!(!sent[0].1.contains(&resume_word()), "{:?}", sent[0].1);
    let lines = lines_about(&host, sid);
    assert!(
        lines.iter().any(|(e, l)| e == "quota.moving" && l.contains("refused Fable")),
        "{lines:?}"
    );
    assert!(plugin.fable_need.marked(sid));
}

/// 2026-11-21/22: Opus panes moved P1 → P5 and P1 → P6 "to leave Fable
/// to the panes that need it". A pane that has been refused Fable is
/// one of those panes.
#[test]
fn a_pane_refused_fable_is_not_moved_off_to_leave_fable_to_others() {
    let home = quota_home("fable-keep");
    feed(&home, "fable-keep", 1.0, 0.1);
    rebalance(marspot::settings::Rebalance::DryRun);
    confirmed("u-83");
    let host = FakeHost::new(home.join("state"));
    let sid = 83u64;
    let mut on_p2 = pane_on_p1(&home, "u-83");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    let mut plugin = stranded_plugin(on_p2, sid);
    host.set_status(sid, idle_view(Duration::from_secs(600)));

    plugin.fable_need.note(sid, &Ask { at: now(), agent: None }, now());
    // the allocation, told of the mark, judges it a Fable pane and has
    // no move for it onto P1, whose Fable is spent
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    for (s, m) in &plugin.last_meta {
        result.new_meta.insert(*s, m.clone());
    }
    (result.usage, result.choices) = quota_inputs(&result.new_meta);
    let marks: HashMap<u64, i64> = plugin.fable_need.since_by_pane().collect();
    let mut alloc = super::super::allocation_adapter::Allocation::new(super::super::allocation_adapter::events_dir(&marspot::paths::state_root()));
    for at in [now() - 9000, now()] {
        alloc.observe(&super::super::allocation_inputs::snapshot(&result, &HashMap::new(), &marks, at));
    }
    let view = alloc.view.take().unwrap();
    assert_eq!(view.panes.iter().find(|p| p.0 == sid).map(|p| p.2), Some(crate::plugins::allocation::Class::Restricted));
    assert!(view.rest.get(&sid).is_none_or(|(to, _, _)| *to != 1), "{:?}", view.rest);
    result.alloc = Some(view);
    host.logged.lock().unwrap().clear();
    plugin.sweep_unusable_profiles(&host, &result);
    assert!(lines_about(&host, sid).is_empty(), "{:?}", lines_about(&host, sid));
    assert!(!plugin.rebalance_said.contains_key(&sid));
    assert!(host.submitted.lock().unwrap().is_empty());
}

/// Nowhere has Fable: the pane stays, and says so once.
#[test]
fn with_fable_spent_everywhere_a_refused_pane_stays_and_says_so_once() {
    let home = quota_home("fable-none");
    feed(&home, "fable-none", 0.5, 1.0);
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-84");
    let host = FakeHost::new(home.join("state"));
    let sid = 84u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-84"), sid);

    let mut refused = agent_refused_fable(sid);
    for _ in 0..3 {
        sweep(&mut plugin, &host, &mut refused);
    }
    assert!(host.submitted.lock().unwrap().is_empty(), "not moved");
    let lines = lines_about(&host, sid);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, "quota.fable_nowhere", "{lines:?}");
}

/// Background work runs when the refusal is read: the move ends it,
/// and the session hears both what to start again and that the rest
/// was ended.
#[test]
fn a_fable_move_that_ends_background_work_says_both() {
    let home = quota_home("fable-work");
    feed(&home, "fable-work", 0.5, 0.1);
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-85");
    let host = FakeHost::new(home.join("state"));
    let sid = 85u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-85"), sid);

    let mut refused = agent_refused_fable(sid);
    refused.new_vetoes.insert(sid, (true, false));
    sweep(&mut plugin, &host, &mut refused);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(plugin.fable_need.word(sid, 1, 2), None, "told once");
    let said = &sent[0].1;
    assert!(said.contains("重新启动这个子代理"), "{said:?}");
    assert!(said.contains("后台任务和后台子代理已经被结束了"), "{said:?}");
    assert!(said.find("Design RFC") < said.find("后台任务和后台子代理"), "{said:?}");
}

/// The feed has nothing on P1, so only the refusal can say it is out:
/// a background agent's refusal of Fable is enough for a marked pane.
#[test]
fn an_agent_refusal_moves_a_marked_pane_off_an_account_the_feed_does_not_know() {
    let home = quota_home("fable-unknown");
    fs::write(
        usage_feed(&home),
        r#"{"generated_at": "2026-11-20T00:00:00+00:00", "accounts": [
           {"name":"B","email":"fable-unknown2@example","status":"allowed","utilization_5h":null,
            "utilization_7d":0.3,"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x",
            "collected_at":"2026-11-20T00:00:00+00:00",
            "model_limits":[{"label":"Fable","utilization":0.1,"reset":4102444900}]}]}"#,
    )
    .unwrap();
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-86");
    let host = FakeHost::new(home.join("state"));
    let sid = 86u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-86"), sid);

    sweep(&mut plugin, &host, &mut agent_refused_fable(sid));
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{:?}", lines_about(&host, sid));
    assert!(sent[0].1.contains(".claude-profile-2"), "{:?}", sent[0].1);
}

/// A shell restart, as after a silent update, keeps the mark: the pane
/// still counts as one that needs Fable.
#[test]
fn a_fable_mark_survives_the_shell_restarting() {
    let home = quota_home("fable-reload");
    feed(&home, "fable-reload", 0.5, 0.1);
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-87");
    let host = FakeHost::new(home.join("state"));
    let sid = 87u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-87"), sid);
    sweep(&mut plugin, &host, &mut agent_refused_fable(sid));
    assert!(plugin.fable_need.marked(sid));

    let mut again = stranded_plugin(pane_on_p1(&home, "u-87"), sid);
    assert!(!again.fable_need.marked(sid));
    again.load_fable_need(&host);
    assert!(again.fable_need.marked(sid));
    assert_eq!(again.fable_need.word(sid, 1, 2), None, "it was told before the restart");
}

/// Moved to P2 for its refusal and told; later the feed reads P2's
/// Fable spent. The next move is for the feed, not a refusal.
#[test]
fn a_marked_pane_moved_again_for_the_feed_does_not_say_it_was_refused() {
    let home = quota_home("fable-feed");
    feed(&home, "fable-feed", 0.1, 1.0);
    rebalance(marspot::settings::Rebalance::Off);
    confirmed("u-88");
    let host = FakeHost::new(home.join("state"));
    let sid = 88u64;
    let mut on_p2 = pane_on_p1(&home, "u-88");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    let mut plugin = stranded_plugin(on_p2, sid);
    plugin.fable_need.note(sid, &Ask { at: now() - 600, agent: None }, now() - 600);
    plugin.fable_need.told(sid);

    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    let lines = lines_about(&host, sid);
    let moving: Vec<_> = lines.iter().filter(|(e, _)| e == "quota.moving").collect();
    assert_eq!(moving.len(), 1, "{lines:?}");
    assert!(moving[0].1.contains("usage feed shows its Fable spent"), "{lines:?}");
    assert!(!moving[0].1.contains("refused"), "{lines:?}");
}
