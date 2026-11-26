//! A badge click: what it starts, and what it refuses to start.

use super::*;

/// Wire a badge click against a session one background job holds,
/// with that job's CLI `status`, and return the badge the click put
/// up after ONE tick.
///
/// One tick only, on purpose: a cycle that got as far as
/// `stop_background` would signal the holder.
/// The first step of every cycle is a timed settle, so a single tick
/// cannot reach a signal — what it can do is show which op the click
/// queued, and that is the whole question.
fn badge_after_click_on_bg_held_session(tag: &str, status: &str) -> String {
    let home = TestHome::new(&format!("cc-cycle-gate-{tag}"));
    // Two profiles so the cycle has somewhere to go: P1 → P2.
    for n in [1u8, 2] {
        fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions")))
            .unwrap();
    }
    let uuid = "e1e62fd9-d0bc-493a-865a-56f2de8d7ac3";
    // a holder counts only if it is a live claude
    let holder = spawn_fake_claude(&home);
    let me = holder.id() as i32;
    // The shape claude actually writes, taken off a real `claude
    // --bg` run: `kind` is **"bg"**, not "background".  The test
    // being `!= "interactive"` rather than a list of known kinds
    // is what makes that not matter — and this record is here so
    // it keeps not mattering.
    fs::write(
        home.join(format!(".claude-profile-2/sessions/{me}.json")),
        format!(
            r#"{{"pid":{me},"sessionId":"{uuid}","cwd":"/tmp","kind":"bg","status":"{status}"}}"#
        ),
    )
    .unwrap();

    let host = FakeHost::new(home.join("state"));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    let meta = BindMeta {
        bound_by: "test",
        profile_num: 1,
        config_dir: Some(
            home.join(".claude-profile-1").to_string_lossy().into_owned(),
        ),
        uuid: uuid.to_string(),
        // Never signalled within one tick: the cycle re-checks the
        // claude before acting, so it has to be a live one.
        claude: live_claude(),
        project_basename: String::new(),
        project_dir: None,
        refused_at: None,
        transcript_at: SystemTime::now(),
        has_transcript: true,
    };

    plugin.start_profile_cycle_to(&host, 7, meta, 2, None, known());
    host.pump_ops();

    // The badge rides the PaneSession, so it takes a tick to
    // appear — the real host drives these from its own loop.
    struct Recording(std::sync::Mutex<Vec<String>>);
    impl crate::plugins::PaneSessionHost for Recording {
        fn shelld_session_id(&self) -> u64 {
            7
        }
        fn end(&self) {}
        fn set_badge(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_string());
        }
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    for sess in host.sessions.lock().unwrap().iter_mut() {
        sess.on_tick(&rec);
    }
    let badges = rec.0.lock().unwrap().clone();
    drop(holder);
    assert_eq!(badges.len(), 1, "one badge, for the one click: {badges:?}");
    badges[0].clone()
}

/// A busy background job keeps its session: the click is answered
/// by the refusal, because stopping the job would cut off its work.
#[test]
fn a_click_on_a_busy_bg_held_session_refuses_instead_of_cycling() {
    let badge = badge_after_click_on_bg_held_session("busy", "busy");
    // A running step gets a spinner frame appended — kept, because
    // it is what says the click was received at all.
    assert!(
        badge.starts_with("⚠ held by bg job"),
        "a busy job must not be stopped by a badge click; `→ P2` here would \
         mean the cycle ran.  got {badge:?}"
    );
}

/// An idle background job no longer blocks the switch: the cycle runs
/// (and stops the job before resuming — pinned in
/// `a_cycle_through_a_background_hold_stops_it_before_resuming`).
/// Asked for on 2026-09-17: "尽可能 switch 完成以后要能恢复".
#[test]
fn a_click_on_an_idle_bg_held_session_cycles() {
    let badge = badge_after_click_on_bg_held_session("idle", "idle");
    assert!(
        badge.starts_with("→ P2"),
        "an idle job's session should switch and come back in the pane; got {badge:?}"
    );
}

/// And the other half: with nothing holding the session, the same
/// click still cycles.
///
/// A gate that is only ever tested when it fires is a gate that can
/// quietly refuse everything.  Same setup as above with the record
/// changed to `interactive`, which is what a pane running claude in
/// the foreground writes about itself — the ordinary case, and the
/// one the feature exists for.
#[test]
fn a_click_on_an_unheld_session_still_cycles() {
    let home = TestHome::new("cc-cycle-open");
    for n in [1u8, 2] {
        fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions")))
            .unwrap();
    }
    let uuid = "e1e62fd9-d0bc-493a-865a-56f2de8d7ac3";
    let me = std::process::id() as i32;
    fs::write(
        home.join(format!(".claude-profile-2/sessions/{me}.json")),
        format!(
            r#"{{"pid":{me},"sessionId":"{uuid}","cwd":"/tmp","kind":"interactive","status":"idle"}}"#
        ),
    )
    .unwrap();

    let host = FakeHost::new(home.join("state"));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    let meta = BindMeta {
        bound_by: "test",
        profile_num: 1,
        config_dir: Some(
            home.join(".claude-profile-1").to_string_lossy().into_owned(),
        ),
        uuid: uuid.to_string(),
        // The cycle's first steps do not signal, so nothing is
        // signalled during this test; the claude has to be live
        // because the cycle re-checks it before acting.
        claude: live_claude(),
        project_basename: String::new(),
        project_dir: None,
        refused_at: None,
        transcript_at: SystemTime::now(),
        has_transcript: true,
    };

    plugin.start_profile_cycle_to(&host, 7, meta, 2, None, known());
    host.pump_ops();

    struct Recording(std::sync::Mutex<Vec<String>>);
    impl crate::plugins::PaneSessionHost for Recording {
        fn shelld_session_id(&self) -> u64 {
            7
        }
        fn end(&self) {}
        fn set_badge(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_string());
        }
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    for sess in host.sessions.lock().unwrap().iter_mut() {
        sess.on_tick(&rec);
    }

    let badges = rec.0.lock().unwrap();
    assert_eq!(badges.len(), 1);
    assert!(
        badges[0].starts_with("→ P2"),
        "an unheld session must still cycle. got {:?}",
        badges[0]
    );
    drop(badges);
}
