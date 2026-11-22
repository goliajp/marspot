//! The sweep that moves a pane off a spent account, or onto quota that would lapse.

use super::*;

/// An automatic switch does not go ahead on a guess about the pane:
/// its model has to be known, and the hook's record of what to
/// start the new claude on has to name the same one.
#[test]
fn a_pane_is_not_moved_until_its_model_is_confirmed() {
    let home = quota_home("confirm");
    let host = FakeHost::new(home.join("state"));
    let sid = 81u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-81"), sid);

    let mut unknown = scan_of(sid, CcActivity::AwaitingUser);
    unknown.new_models.clear();
    sweep(&mut plugin, &host, &mut unknown);
    assert!(host.submitted.lock().unwrap().is_empty(), "model not known");
    assert!(plugin.stranded.is_empty(), "not even judged: it takes no part");

    fs::write(model_push_dir().join("u-81"), "opus-5-5\n/t/x.jsonl\n\npids=1\nid=claude-opus-5-5\n")
        .unwrap();
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert!(host.submitted.lock().unwrap().is_empty(), "effort not known");
    assert!(plugin.stranded.is_empty(), "not judged either");

    let mut other = scan_of(sid, CcActivity::AwaitingUser);
    other.new_models.insert(sid, "fable-5-1".into());
    sweep(&mut plugin, &host, &mut other);
    assert!(host.submitted.lock().unwrap().is_empty(), "the record names another model");

    fs::remove_file(model_push_dir().join("u-81")).unwrap();
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert!(host.submitted.lock().unwrap().is_empty(), "no record to start the new claude from");

    fs::write(
        model_push_dir().join("u-81"),
        "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n",
    )
    .unwrap();
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    let handed = host.submitted.lock().unwrap().clone();
    assert_eq!(handed.len(), 1, "both reads agree: it moves");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains("--model 'claude-opus-5-5'"), "{:?}", sent[0].1);
}

/// P2 has room by the feed, which cannot tell an account nobody has
/// used for a while from one that is logged out. A pane on P2 has
/// just been told it cannot log in. It leaves, and nothing is sent
/// there in its place: the pane on the spent P1 stays put.
#[test]
fn a_profile_a_pane_could_not_log_in_to_is_no_destination() {
    let home = quota_home("login");
    let host = FakeHost::new(home.join("state"));
    let mut on_p2 = pane_on_p1(&home, "u-78");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    on_p2.refused_at = Some(quota::Refusal { at: 1_000, model: String::new(), logged_out: true, window: quota::Window::Week });
    let mut plugin = stranded_plugin(on_p2, 78);
    plugin.last_meta.insert(79, pane_on_p1(&home, "u-79"));
    let mut result = scan_of(78, CcActivity::AwaitingUser);
    result.sessions_seen.push(79);
    result.new_activity.insert(79, CcActivity::AwaitingUser);
    result.new_vetoes.insert(79, (false, false));
    result.new_models.insert(79, "opus-5-5".into());

    sweep(&mut plugin, &host, &mut result);
    assert_eq!(plugin.refused.logged_out.get(&2), Some(&1_000));
    assert!(host.submitted.lock().unwrap().is_empty(), "P1 is spent and P2 cannot log in");
    assert_eq!(plugin.stranded.get(&79).and_then(|s| s.hold_said), Some("every account is out"));
    assert_eq!(plugin.stranded.get(&78).and_then(|s| s.hold_said), Some("every account is out"));
}

/// The reason a pane is held is said again when it changes, so the
/// log shows what kept it there all along.
#[test]
fn a_new_reason_for_holding_is_said_again() {
    let home = quota_home("hold");
    let host = FakeHost::new(home.join("state"));
    // both accounts spent
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        r#"{"generated_at": "2026-09-29T00:00:00+00:00", "accounts": [
               {"name":"A","email":"hold1@example","status":"rejected","utilization_5h":0.0,
                "utilization_7d":1.0,"reset_5h":1,"reset_7d":4102444800},
               {"name":"B","email":"hold2@example","status":"rejected","utilization_5h":0.0,
                "utilization_7d":1.0,"reset_5h":1,"reset_7d":4102444900}]}"#,
    )
    .unwrap();
    let sid = 80u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-80"), sid);
    let said = |p: &ClaudecodePlugin| p.stranded.get(&sid).and_then(|s| s.hold_said);

    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::ToolPending { executing: true }));
    assert_eq!(said(&plugin), Some("a tool is running here"));
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert_eq!(said(&plugin), Some("every account is out"));
    assert!(host.submitted.lock().unwrap().is_empty());
}

/// The wiring, end to end inside the process: a pane sitting on an
/// account that is out gets moved to one that is not.
///
/// Nothing here reads a screen or greps a transcript for the words
/// a refusal prints.  The account says it cannot answer, the pane
/// is on that account, and that is the whole trigger.
#[test]
fn a_pane_on_a_spent_account_is_handed_to_the_host_as_a_switch() {
    let home = quota_home("wire");
    let host = FakeHost::new(home.join("state"));
    let sid = 77u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-77"), sid);
    // Waiting for its person as far as the transcript can tell —
    // which is also what a turn the account just killed looks
    // like, so it is spoken to all the same.
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    sweep(&mut plugin, &host, &mut result);

    let handed = host.submitted.lock().unwrap().clone();
    assert_eq!(handed.len(), 1, "one switch for one stranded pane: {handed:?}");
    assert_eq!(handed[0].0, sid);
    assert!(handed[0].1.contains(&"resume"), "{:?}", handed[0].1);
    let sent = host.sent.lock().unwrap().clone();
    assert!(
        sent[0].1.contains(&resume_word()),
        "every moved pane is asked to carry on — the reading of \"idle\" was \
         wrong for three of six panes on 2026-09-30 — and it is asked on \
         claude's own command line, never through the person's composer: \
         {:?}",
        sent[0].1
    );

    // The next pass comes round before the resume has finished, so
    // the pane still reads as being on P1.  It is not moved a
    // second time on top of the first.
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "one move per stranding");

    // And not even when the pane is briefly unbound in between —
    // which is what the cycle itself causes, and where the tally
    // used to be thrown away (pane 442, 2026-09-29).
    plugin.last_meta.remove(&sid);
    sweep(&mut plugin, &host, &mut result);
    plugin.last_meta.insert(sid, pane_on_p1(&home, "u-77"));
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(
        host.submitted.lock().unwrap().len(),
        1,
        "a pane between bindings is still a pane that was just moved"
    );
}

/// A resting Opus pane on an account that still has Fable moves to
/// one whose Fable is spent: logged only under `dryrun`, moved
/// under `on`, and moved without being told to carry on, because
/// nothing was interrupted.  Then it is not moved again while the
/// move is on its way.
#[test]
fn a_resting_pane_moves_where_its_quota_is_worth_least_to_others() {
    // the move waits out an overloaded machine, and this one reads
    // the real load
    if !marspot::host_load::quiet_enough_to_time("a resting pane's move") {
        return;
    }
    let home = quota_home("rest");
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        r#"{"generated_at": "2026-11-05T00:00:00+00:00", "accounts": [
               {"name":"A","email":"rest1@example","status":"rejected","utilization_5h":null,
                "utilization_7d":0.98,"reset_5h":null,"reset_7d":4102444800,"tier":"default_claude_max_20x",
                "model_limits":[{"label":"Fable","utilization":1.0,"reset":4102444800}]},
               {"name":"B","email":"rest2@example","status":"allowed","utilization_5h":null,
                "utilization_7d":0.02,"reset_5h":null,"reset_7d":4102444900,"tier":"default_claude_max_20x",
                "model_limits":[{"label":"Fable","utilization":0.0,"reset":4102444900}]}]}"#,
    )
    .unwrap();
    let host = FakeHost::new(home.join("state"));
    let sid = 81u64;
    let mut on_p2 = pane_on_p1(&home, "u-81");
    on_p2.profile_num = 2;
    on_p2.config_dir = Some(home.join(".claude-profile-2").to_string_lossy().into_owned());
    let mut plugin = stranded_plugin(on_p2, sid);
    host.status.lock().unwrap().insert(
        sid,
        crate::plugins::PaneStatusView {
            status: marspot::pane_state::PaneStatus::AwaitingUser,
            held: Duration::from_secs(600),
            quiescent: true,
        },
    );
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    let mode = |m| {
        marspot::settings::set_for_test(marspot::settings::Settings {
            cc_rebalance: m,
            ..Default::default()
        })
    };

    mode(marspot::settings::Rebalance::DryRun);
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty(), "dryrun moves nothing");

    mode(marspot::settings::Rebalance::Off);
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty());

    // the user is in it
    mode(marspot::settings::Rebalance::On);
    plugin.focused_sid = Some(sid);
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty(), "not under the user's cursor");
    plugin.focused_sid = None;

    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "moved");
    let sent = host.sent.lock().unwrap().clone();
    assert!(sent[0].1.contains(".claude-profile-1"), "to P1: {:?}", sent[0].1);
    assert!(!sent[0].1.contains(&resume_word()), "nothing to carry on with: {:?}", sent[0].1);
    assert_eq!(plugin.rebalanced.get(&sid).map(|(_, to)| *to), Some(1));

    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "not again while it lands");
}

/// A turn visibly in flight gets the same treatment — the point of
/// this one is that nothing about the wiring depends on which of
/// the two readings the transcript gave.
#[test]
fn a_pane_that_was_working_is_told_to_carry_on() {
    let home = quota_home("say");
    let host = FakeHost::new(home.join("state"));
    let sid = 78u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-78"), sid);
    // A turn was being produced when the account stopped answering.
    let mut result = scan_of(sid, CcActivity::Working);
    sweep(&mut plugin, &host, &mut result);

    let handed = host.submitted.lock().unwrap().clone();
    assert_eq!(handed.len(), 1);
    let sent = host.sent.lock().unwrap().clone();
    assert!(
        sent[0].1.contains(&resume_word()),
        "a pane that was working is told to carry on: {:?}",
        sent[0].1
    );
}

/// A pane put somewhere by hand is judged like any other: by its
/// model against what that account has left for it. And the switch
/// itself waits until the pane's model is confirmed.
#[test]
fn a_switch_by_hand_follows_the_same_rule() {
    let home = quota_home("hand");
    let host = FakeHost::new(home.join("state"));
    let sid = 80u64;
    let on = |profile: u8| {
        let mut m = pane_on_p1(&home, "u-80");
        m.profile_num = profile;
        m.config_dir =
            Some(home.join(format!(".claude-profile-{profile}")).to_string_lossy().into_owned());
        m
    };
    let mut plugin = stranded_plugin(on(2), sid);
    let mut result = scan_of(sid, CcActivity::AwaitingUser);
    let switches = |host: &FakeHost| {
        host.submitted.lock().unwrap().iter().filter(|(_, steps)| steps.contains(&"resume")).count()
    };

    // Model not read yet: the pick is refused, nothing is started.
    plugin.on_pane_badge_menu_action(&host, sid, crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, 1));
    assert_eq!(switches(&host), 0, "the model is not known yet");
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "the badge says why");

    plugin.last_model.insert(sid, "opus-5-5".into());
    plugin.on_pane_badge_menu_action(&host, sid, crate::plugins::handoff::profile_tag(crate::plugins::handoff::Agent::Claude, 1));
    assert_eq!(switches(&host), 1, "the pick itself");
    assert!(
        host.sent.lock().unwrap().iter().any(|(_, l)| l.contains("--model 'claude-opus-5-5'")),
        "started on the model it was on"
    );
    assert!(
        host.sent.lock().unwrap().iter().any(|(_, l)| l.contains(&resume_word())),
        "told the same as after an automatic move"
    );

    // Landed on P1, which has no room for it: it moves on, to P2.
    plugin.last_meta.insert(sid, on(1));
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(switches(&host), 2, "unusable is unusable, however it got there");

    // On P2, which has room, nothing happens however often it looks.
    plugin.last_meta.insert(sid, on(2));
    sweep(&mut plugin, &host, &mut result);
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(switches(&host), 2, "usable: left alone");
}

/// A pane whose session has no name yet cannot be moved — there is
/// nothing to resume on the other side, and starting fresh would
/// drop what is on the screen.  The attempt must not be recorded
/// as a move: the pane never went anywhere, and spending the
/// budget on it leaves the real move with nothing to spend.
#[test]
fn a_pane_with_no_session_to_resume_is_not_counted_as_moved() {
    let home = quota_home("nouuid");
    let host = FakeHost::new(home.join("state"));
    let sid = 79u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, ""), sid);
    let mut result = scan_of(sid, CcActivity::Working);
    sweep(&mut plugin, &host, &mut result);
    assert!(host.submitted.lock().unwrap().is_empty(), "nothing to hand over");
    assert_eq!(
        plugin.stranded.get(&sid).map_or(0, |s| s.moves),
        0,
        "no move was spent -- with no session name there is no hook record, so it is not even judged"
    );

    // Once the scan learns the session's name, the move happens.
    plugin.last_meta.insert(sid, pane_on_p1(&home, "u-79"));
    sweep(&mut plugin, &host, &mut result);
    assert_eq!(host.submitted.lock().unwrap().len(), 1);
}
