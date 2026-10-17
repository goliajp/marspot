//! Acting only on the claude a decision was made about, and letting a
//! pane go when its wake fails.

use super::*;

/// A claude that replaced the one the cycle took down must not get the
/// resume line typed into its prompt: the cycle checks right before it
/// types, as the reclaim does.
#[test]
fn a_cycle_checks_for_a_claude_right_before_typing() {
    let op = profile_cycle_op(true, "u-1", true, 4, fid(4242), Some(fid(5151)), 4200, None, &CliChoice::default())
        .expect("cycle");
    let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
    let stop = labels.iter().position(|l| *l == "stop_background").expect("stop step");
    let guard = labels.iter().position(|l| *l == "stop_if_process").expect("guard step");
    let resume = labels.iter().position(|l| *l == "resume").expect("resume step");
    assert!(stop < guard && guard + 1 == resume, "{labels:?}");
    assert!(matches!(op.steps[guard].kind, pty_op::StepKind::StopIfProcess { under: 4200, .. }));
}

/// The automatic move waits while claude has work of its own running,
/// whatever the transcript says, and goes once it has none.
#[test]
fn a_quota_move_waits_for_work_in_flight() {
    let home = quota_home("inflight");
    let host = FakeHost::new(home.join("state"));
    let sid = 77u64;
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-77"), sid);

    let mut busy = scan_of(sid, CcActivity::AwaitingUser);
    busy.new_vetoes.insert(sid, (true, false));
    sweep(&mut plugin, &host, &mut busy);
    assert!(host.submitted.lock().unwrap().is_empty(), "work in flight: not moved");
    assert_eq!(plugin.stranded.get(&sid).and_then(|s| s.hold_said), Some("work is in flight here"));

    let mut unknown = scan_of(sid, CcActivity::AwaitingUser);
    unknown.new_vetoes.clear();
    sweep(&mut plugin, &host, &mut unknown);
    assert!(host.submitted.lock().unwrap().is_empty(), "not known to be idle: not moved");

    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert_eq!(host.submitted.lock().unwrap().len(), 1, "nothing in flight: it moves");
}

/// A pane bound to a claude that has since been replaced is not moved:
/// the move re-checks the process it decided about.
#[test]
fn a_quota_move_rechecks_the_claude() {
    let home = quota_home("recheck");
    let host = FakeHost::new(home.join("state"));
    let sid = 78u64;
    let mut meta = pane_on_p1(&home, "u-78");
    meta.claude.start_us += 1;
    let mut plugin = stranded_plugin(meta, sid);
    sweep(&mut plugin, &host, &mut scan_of(sid, CcActivity::AwaitingUser));
    assert!(host.submitted.lock().unwrap().is_empty(), "another process now: not moved");
}

fn parked_plugin(dir: &std::path::Path, sid: u64) -> (FakeHost, ClaudecodePlugin) {
    fs::create_dir_all(dir).unwrap();
    let host = FakeHost::new(dir.to_path_buf());
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.armed.insert(sid);
    plugin.dormant.push(DormantRecord {
        shelld_sid: sid,
        uuid: "u-park".into(),
        profile_num: 2,
        config_dir: None,
        project_dir: None,
        choice: CliChoice::default(),
        created_at: std::time::UNIX_EPOCH,
    });
    plugin.held.insert(sid, FrozenLook { badge: "P2 park".into(), title: "proj".into() });
    (host, plugin)
}

fn badges_for(host: &FakeHost, sid: u64) -> Vec<String> {
    host.pane_badges.lock().unwrap().iter().filter(|(s, _)| *s == sid).map(|(_, b)| b.clone()).collect()
}

/// A wake that ends without claude back lets the pane go: not armed,
/// no dormant record, and the frozen badge is cleared rather than
/// re-asserted over a plain shell.
#[test]
fn a_failed_wake_lets_the_pane_go() {
    let tmp = TempDir::new("wake");
    let dir = tmp.path().to_path_buf();
    let sid = 7u64;
    let (host, mut plugin) = parked_plugin(&dir, sid);
    let scan = scan_of(sid, CcActivity::AwaitingUser);
    plugin.publish_looks(&host, &scan);
    assert_eq!(badges_for(&host, sid), vec!["P2 park".to_string()], "parked: the look is held");

    (plugin.wake_reporter(sid))(&pty_op::OpOutcome::TimedOut { step: 7, label: "await_process" });
    plugin.rearm_dormant(&host, &scan);
    assert!(!plugin.armed.contains(&sid));
    assert!(plugin.dormant.is_empty());
    assert!(!plugin.held.contains_key(&sid));
    assert!(!fs::read_to_string(dir.join("dormant.tsv")).unwrap_or_default().contains("u-park"));
    // the scan no longer maps the pane, so letting it go is the one
    // thing that clears its corner
    assert_eq!(
        badges_for(&host, sid),
        vec!["P2 park".to_string(), String::new()],
        "the frozen badge is cleared when the wake fails"
    );

    host.pane_badges.lock().unwrap().clear();
    plugin.publish_looks(&host, &scan);
    assert!(
        badges_for(&host, sid).is_empty(),
        "and the next publish does not put it back over a plain shell: {:?}",
        badges_for(&host, sid)
    );
}

/// A run ended from outside is not a failed wake: the pane may still be
/// parked, so only the arming goes and the re-arm sweep judges it.
#[test]
fn a_cancelled_wake_keeps_the_pane_parked() {
    let tmp = TempDir::new("wake");
    let dir = tmp.path().to_path_buf();
    let sid = 7u64;
    let (host, mut plugin) = parked_plugin(&dir, sid);
    (plugin.wake_reporter(sid))(&pty_op::OpOutcome::Cancelled("plugin"));
    plugin.settle_wake_ends(&host);
    assert!(!plugin.armed.contains(&sid));
    assert_eq!(plugin.dormant.len(), 1);
    assert!(plugin.held.contains_key(&sid));
}

/// A frozen look for a pane that no longer exists is dropped.
#[test]
fn a_held_look_goes_with_its_pane() {
    let tmp = TempDir::new("wake");
    let dir = tmp.path().to_path_buf();
    let (host, mut plugin) = parked_plugin(&dir, 7);
    plugin.held.insert(8, FrozenLook { badge: "P1".into(), title: String::new() });
    plugin.rearm_dormant(&host, &scan_of(7, CcActivity::AwaitingUser));
    assert!(plugin.held.contains_key(&7));
    assert!(!plugin.held.contains_key(&8));
}

/// A CPU sample taken of another process is no baseline: the claude
/// that replaced it started its counter low, which would read as idle.
#[test]
fn a_cpu_baseline_from_another_process_is_no_evidence() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let child = spawn_fake_claude(&dir);
    let pid = child.id() as i32;
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

    let scan = scan_with(7, pid, "aaaa-cccc");
    let now_id = scan.new_meta[&7].claude;
    let before = pidtree::ProcId { pid: now_id.pid, start_us: now_id.start_us - 1 };
    plugin.cpu_samples.insert(7, (before, 1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    assert!(plugin.dormant.is_empty(), "the sample was of another process");
    assert!(host.submitted.lock().unwrap().is_empty());
    assert_eq!(plugin.cpu_samples.get(&7).map(|(id, ..)| *id), Some(now_id), "a new baseline");

    // the same process, sampled long enough ago, is evidence
    plugin.cpu_samples.insert(7, (now_id, 1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan_with(7, pid, "aaaa-cccc"), TEST_THRESHOLD);
    assert_eq!(plugin.dormant.len(), 1, "and with it the pane is reclaimed");
    drop(child);
}
