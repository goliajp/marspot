//! The reclamation itself, run against a real process.

use super::*;

// ── the reclamation itself, against a real process ────────────


/// The freeze lifts when claude has finished drawing, not when its
/// process appears.
///
/// Those are seconds apart, and ending on "the process exists" is
/// what the user saw: the pane unfroze onto the echoed
/// `claude --resume …` line and the startup output scrolling by,
/// The wake waits for claude to finish *painting*, and only counts
/// output that arrives after claude exists.
///
/// Both halves shipped broken.  Ending on "the process exists"
/// unfroze the pane onto the echoed resume line and the startup
/// scroll; then counting ticks instead of milliseconds made the
/// still-window 48 ms (`on_tick` rides the redraw pump: 16 ms while
/// a pane is drawing, ~250 ms when the window is idle), which lands
/// inside claude's own startup pause.  The rule itself now lives in
/// `pty_op::StepKind::AwaitQuiet`; what stays claudecode's to choose
/// is how long, and that it comes after the process check.
#[test]
fn the_wake_waits_for_the_repaint_after_the_process_appears() {
    let op = reclaim_op("u", None, Some(fid(1)), 2, None, &CliChoice::default()).expect("script builds");
    let kinds: Vec<&pty_op::StepKind> = op.steps.iter().map(|s| &s.kind).collect();
    let process_at = kinds
        .iter()
        .position(|k| matches!(k, pty_op::StepKind::AwaitProcess { .. }))
        .expect("waits for claude to come back");
    // not the shell's fresh prompt, awaited before the line is typed
    let quiet_at = op
        .steps
        .iter()
        .position(|s| matches!(s.kind, pty_op::StepKind::AwaitQuiet { .. }) && s.label != "fresh_prompt")
        .expect("waits for it to finish drawing");
    assert!(
        process_at < quiet_at,
        "output before claude exists is the shell's, not claude's"
    );
    assert!(
        matches!(
            kinds[quiet_at],
            pty_op::StepKind::AwaitQuiet { still, min_bytes, .. }
                if *still >= Duration::from_millis(400) && *min_bytes > 0
        ),
        "a still-window measured in tens of ms lands inside claude's \
         startup pause, and silence alone lands inside its load"
    );
}

/// The 2026-08-03 report: a pane running claudecode with no badge
/// at all.  claude writes its session file on the first turn, so
/// between `claude` starting and the user's first prompt there was
/// nothing to bind to and the corner stayed empty for minutes.
///
/// The profile is readable from the process the whole time, so
/// that window badges `P<n>` and gains `@model` once the session
/// file shows up.  What such a pane must NOT do is get taken down
/// and resumed — there is no session to resume.
#[test]
fn a_pane_with_no_session_file_yet_is_badged_but_never_reclaimed() {
    assert!(
        reclaim_op("", Some("/Users/x/.claude-profile-2"), Some(fid(4242)), 4200, None, &CliChoice::default()).is_none(),
        "no uuid ⇒ no resume line ⇒ claude must not be taken down"
    );
    assert!(
        profile_cycle_op(Some(&resume_word()), "", false, 3, fid(4242), None, 4200, None, &CliChoice::default()).is_none(),
        "…and the badge-click cycle refuses for the same reason"
    );
    // The same call with a uuid is the normal path, so the guard
    // above is the only thing being tested here.
    assert!(reclaim_op("u-1", None, Some(fid(4242)), 4200, None, &CliChoice::default()).is_some());
    assert!(profile_cycle_op(Some(&resume_word()), "u-1", true, 3, fid(4242), None, 4200, None, &CliChoice::default()).is_some());
}

/// …and the idle policy stops before signalling such a pane, with
/// a reason, rather than building a broken resume line.
#[test]
fn the_idle_policy_leaves_an_unbound_pane_running() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let mut child = spawn_fake_claude(&dir);
    let pid = child.id() as i32;
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));

    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

    // Two passes — the second is the one that would decide.
    let mut scan = scan_with(7, pid, "");
    scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    let scan = scan_with(7, pid, "");
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    host.pump_ops();

    assert!(host.begun.lock().unwrap().is_empty(), "nothing was reclaimed");
    assert!(plugin.dormant.is_empty());
    assert_eq!(
        plugin.blocked_reason.get(&7).map(String::as_str),
        Some("unknown_session"),
        "and it says why"
    );
    assert!(
        !matches!(child.try_wait(), Ok(Some(_))),
        "the process is left alone"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// 2026-08-03 report: the reclamation is visible.
///
/// The picture stands still, but killing claude drops the pane out
/// of the scan's mapping — and the mapping is what clears badges.
/// A parked pane blanked its corner and then wore `zZ`, which is
/// the reclamation announcing itself on a pane whose whole point is
/// that nothing about it moved.
#[test]
fn a_held_pane_keeps_the_badge_it_was_frozen_with() {
    let tmp = TempDir::new("look");
    let dir = tmp.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    let mut plugin = ClaudecodePlugin::new();

    // Two panes wore a badge last tick; one of them is now held.
    plugin.last_mapping.insert(7, "P2@opus-5".to_string());
    plugin.last_mapping.insert(8, "P1@fable-5".to_string());
    plugin.held.insert(
        7,
        FrozenLook { badge: "P2@opus-5".into(), title: "proj".into() },
    );

    // This scan sees neither: pane 7 because we killed its claude,
    // pane 8 because the user quit it themselves.
    plugin.publish_looks(&host, &empty_scan());

    let badges = host.pane_badges.lock().unwrap().clone();
    assert!(
        badges.contains(&(7, "P2@opus-5".to_string())),
        "the held pane keeps exactly what it was frozen with: {badges:?}"
    );
    assert!(
        !badges.iter().any(|(sid, text)| *sid == 7 && text.is_empty()),
        "and is never blanked: {badges:?}"
    );
    assert!(
        badges.contains(&(8, String::new())),
        "a pane that lost claude on its own still clears: {badges:?}"
    );
    assert!(
        host.pane_titles
            .lock()
            .unwrap()
            .contains(&(7, "proj".to_string())),
        "its title is frozen too"
    );
}

fn empty_scan() -> ScanResult {
    ScanResult {
        new_mapping: HashMap::new(),
        new_models: HashMap::new(),
        new_meta: HashMap::new(),
        new_activity: HashMap::new(),
        new_cpu: HashMap::new(),
        new_vetoes: HashMap::new(),
        agent_refused: HashMap::new(),
        fable_refused: HashMap::new(),
        helpers: Default::default(),
        pane_tokens: HashMap::new(),
        speeds: Default::default(),
        alloc: None,
        scanned_at: SystemTime::now(),
        sessions_seen: Vec::new(),
        log_lines: Vec::new(),
        profiles: Vec::new(),
        usage: None,
        choices: HashMap::new(),
    }
}

/// The whole reclamation path, with a real process on the other
/// end of the signal: first pass only samples CPU, second pass
/// decides, signals, and records the pane as dormant.
/// Reclaiming restarts claude, and a pane whose model or effort is
/// not known would wake on its profile's default. So it is left
/// running, however idle it is.
#[test]
fn a_pane_whose_model_is_not_known_is_not_reclaimed() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let mut child = spawn_fake_claude(&dir);
    let pid = child.id() as i32;
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    let uuid = "cccc-dddd";

    let unknown = |scan: &mut ScanResult| scan.new_models.clear();
    let mut scan = scan_with(7, pid, uuid);
    unknown(&mut scan);
    scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    let mut scan = scan_with(7, pid, uuid);
    unknown(&mut scan);
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    host.pump_ops();
    assert!(plugin.dormant.is_empty(), "not reclaimed: its model is not known");
    assert!(host.begun.lock().unwrap().is_empty());

    // Known, it is reclaimed as before: the guard is the only
    // difference.
    let scan = scan_with(7, pid, uuid);
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    host.pump_ops();
    assert_eq!(plugin.dormant.len(), 1, "reclaimed once its model is known");
    assert_eq!(
        plugin.dormant[0].choice,
        CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("medium".into()) },
        "and it remembers what to wake on"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn idle_policy_reclaims_a_real_process_and_records_it_dormant() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let mut child = spawn_fake_claude(&dir);
    let pid = child.id() as i32;
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));

    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    let uuid = "aaaa-bbbb";

    // Pass 1: no previous CPU sample, so nothing may be decided.
    let mut scan = scan_with(7, pid, uuid);
    scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert!(
        host.begun.lock().unwrap().is_empty(),
        "the first CPU sample cannot decide anything"
    );
    assert!(plugin.dormant.is_empty());

    // Pass 2: same CPU total, sample window wide enough.
    let scan = scan_with(7, pid, uuid);
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert!(
        host.begun.lock().unwrap().is_empty(),
        "the decision queues the op; starting it is the service's job"
    );
    // …which happens on the next tick, exactly as in production.
    host.pump_ops();
    assert_eq!(*host.begun.lock().unwrap(), vec![7], "a wake session is armed");
    assert_eq!(plugin.dormant.len(), 1);
    assert_eq!(plugin.dormant[0].uuid, uuid);

    // Nothing has been signalled yet: the session starts in
    // `Holding`, so the picture is safely held before the process
    // is touched.  A kill here would race the hold across two
    // process boundaries and let claude's parting message reach
    // the screen.
    assert!(
        !matches!(child.try_wait(), Ok(Some(_))),
        "the signal must wait for the hold to land"
    );

    // Driving the session's ticks is what sends it — the real host
    // does this from its own loop.
    let session_host = FakePaneSessionHost { sid: 7 };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        for s in host.sessions.lock().unwrap().iter_mut() {
            s.on_tick(&session_host);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(exited, "the fake claude should have been signalled");

    // …and it survives a restart: the record is on disk.
    let text = fs::read_to_string(dir.join("dormant.tsv")).unwrap();
    assert_eq!(decode_dormant(&text).len(), 1);
}

/// The CPU baseline must survive across scans, or the delta always
/// spans one scan (~2 s) and the "sample must span real time" gate
/// can never pass.  That shipped: a pane past the one-hour
/// threshold sat logging `cpu sample spans only 2s`, meaning
/// reclamation would never have fired for anyone.
#[test]
fn the_cpu_baseline_is_held_across_scans_not_replaced_every_time() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    // Below the threshold, so the policy keeps looking rather than
    // reclaiming — which is when the baseline handling matters.
    host.set_status(7, idle_view(Duration::from_secs(10)));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

    let t0 = SystemTime::now() - Duration::from_secs(120);
    // Recent transcript: the session did something a moment ago,
    // so the policy keeps looking rather than reclaiming — which is
    // when the baseline handling is what's under test.
    let recent = |mut scan: ScanResult| -> ScanResult {
        if let Some(m) = scan.new_meta.get_mut(&7) {
            m.transcript_at = SystemTime::now();
        }
        scan
    };
    let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
    scan.new_cpu.insert(7, (1_000, t0));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert_eq!(plugin.cpu_samples.get(&7).map(|(_, c, _)| *c), Some(1_000));

    // A scan two seconds later must NOT move the baseline.
    let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
    scan.new_cpu.insert(7, (2_000, t0 + Duration::from_secs(2)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert_eq!(
        plugin.cpu_samples.get(&7).map(|(_, c, _)| *c),
        Some(1_000),
        "a 2s-old baseline must be kept, or the delta measures nothing"
    );

    // Past the window, it rolls forward.
    let mut scan = recent(scan_with(7, std::process::id() as i32, "u"));
    scan.new_cpu.insert(7, (3_000, t0 + CPU_BASELINE_WINDOW + Duration::from_secs(1)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert_eq!(
        plugin.cpu_samples.get(&7).map(|(_, c, _)| *c),
        Some(3_000),
        "past the window the baseline should move"
    );
}

/// The pid guard: `last_meta` can name a pid that has since been
/// recycled into something else, and signalling that would hit an
/// unrelated process.
#[test]
fn idle_policy_refuses_to_signal_a_pid_that_is_no_longer_claude() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    // A live process that is emphatically not claude: this test
    // process itself.
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    let me = std::process::id() as i32;

    let mut scan = scan_with(7, me, "uuid-1");
    scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    let scan = scan_with(7, me, "uuid-1");
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);

    assert!(
        host.begun.lock().unwrap().is_empty(),
        "nothing may be armed against a pid that isn't claude"
    );
    assert!(plugin.dormant.is_empty());
}

/// A helper, and a pane whose helpers are still working, are left
/// running however long they rest; once the helpers are gone the pane
/// is reclaimed as before.
#[test]
fn idle_policy_leaves_a_helper_and_a_pane_with_live_helpers() {
    use super::super::with_helpers::HelperRoles;
    for roles in [HelperRoles::of([(7, 3)]), HelperRoles::of([(7, 0), (9, 7)])] {
        let sandbox = sandbox_state_dir();
        let dir = sandbox.path().to_path_buf();
        let mut child = spawn_fake_claude(&dir);
        let pid = child.id() as i32;
        let host = FakeHost::new(dir.clone());
        host.set_status(7, idle_view(Duration::from_secs(7200)));
        let mut plugin = ClaudecodePlugin::new();
        plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
        let uuid = "eeee-ffff";
        let mut scan = scan_with(7, pid, uuid);
        scan.helpers = roles.clone();
        scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        let mut scan = scan_with(7, pid, uuid);
        scan.helpers = roles;
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        host.pump_ops();
        assert!(plugin.dormant.is_empty(), "not reclaimed while bound to helpers");
        assert_eq!(plugin.blocked_reason.get(&7).map(String::as_str), Some("helper_bound"));

        // the blocked pass rolled the CPU baseline forward to now
        let mut scan = scan_with(7, pid, uuid);
        scan.new_cpu.insert(7, (1_000, SystemTime::now() + Duration::from_secs(120)));
        plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
        host.pump_ops();
        assert_eq!(plugin.dormant.len(), 1, "reclaimed once no helper is bound to it");
        let _ = child.kill();
        let _ = child.wait();
    }
}
