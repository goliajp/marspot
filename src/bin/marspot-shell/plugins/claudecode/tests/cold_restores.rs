//! Putting claude back in panes whose session process started afresh.

use super::super::cold_record::{LastBound, PaneNow};
use super::super::cold_restore::STAGGER;
use super::*;

#[path = "cold_restore_l3.rs"]
mod cold_restore_l3;

fn row(sid: u64, home: &Path, profile: u8) -> LastBound {
    LastBound {
        sid,
        uuid: format!("u-{sid}"),
        profile,
        config_dir: Some(home.join(format!(".claude-profile-{profile}")).to_string_lossy().into_owned()),
        project_dir: Some("/w/proj".into()),
        choice: CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("high".into()) },
        shell: pidtree::ProcId { pid: 1, start_us: 1 },
    }
}

/// A shell started a moment ago, so inside the window.
fn young_shell() -> pidtree::ProcId {
    let us = (SystemTime::now() - Duration::from_secs(5)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as u64;
    pidtree::ProcId { pid: 4242, start_us: us }
}

fn new_shell(_: u64) -> Option<PaneNow> {
    Some(PaneNow { shell: young_shell(), idle_shell: true, last_input: None })
}

fn unbound_scan(sids: &[u64]) -> ScanResult {
    let mut scan = scan_of(sids[0], CcActivity::Absent);
    scan.new_models.clear();
    scan.sessions_seen = sids.to_vec();
    scan
}

fn plugin_with(rows: Vec<LastBound>) -> ClaudecodePlugin {
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.cold.rows = rows;
    plugin
}

/// The 2026-11-28 case: after the restart pane 483 sat at an empty
/// shell.  Its session comes back on the account and model it ran on.
#[test]
fn a_new_shell_gets_its_claude_back_on_the_account_it_ran_on() {
    let home = TestHome::new("cold-same");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(483, &home, 1)]);
    plugin.cold_restore(&host, &unbound_scan(&[483]), new_shell);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let line = &sent[0].1;
    assert!(!line.contains('\x03'), "nothing is interrupted first: {line:?}");
    let labels = host.submitted.lock().unwrap()[0].1.clone();
    let at = |l: &str| labels.iter().position(|x| *x == l).unwrap();
    assert_eq!(at("untouched") + 1, at("resume"), "checked right before the line: {labels:?}");
    let dir = home.join(".claude-profile-1");
    for want in [format!("CLAUDE_CONFIG_DIR='{}'", dir.display()), "--resume u-483".into(), "--model 'claude-opus-5-5'".into(), "--effort high".into(), "( cd '/w/proj' && ".into()] {
        assert!(line.contains(&want), "{want} missing from {line}");
    }
    let logged = host.logged.lock().unwrap().clone();
    let start = logged.iter().find(|(e, _)| e == "cold_restore.start").expect("a start line");
    assert!(start.1.contains("on P1 (the account it ran on)"), "{}", start.1);
    plugin.cold.last_start = None;
    plugin.cold_restore(&host, &unbound_scan(&[483]), new_shell);
    assert_eq!(host.sent.lock().unwrap().len(), 1, "once per pane in one process");
}

/// One restore per `STAGGER`; a parked pane is the wake's, and a pane
/// already running claude owes nothing.
#[test]
fn restores_are_staggered_and_leave_parked_and_running_panes_alone() {
    let home = TestHome::new("cold-stagger");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with((1..=4).map(|sid| row(sid, &home, 1)).collect());
    plugin.dormant.push(DormantRecord {
        shelld_sid: 2,
        uuid: "u-2".into(),
        profile_num: 1,
        config_dir: None,
        project_dir: None,
        choice: CliChoice::default(),
        created_at: SystemTime::now(),
    });
    let mut scan = unbound_scan(&[1, 2, 3, 4]);
    scan.new_meta.insert(3, pane_on_p1(&home, "u-3"));
    plugin.cold_restore(&host, &scan, new_shell);
    plugin.cold_restore(&host, &scan, new_shell);
    let sids = |h: &FakeHost| h.sent.lock().unwrap().iter().map(|(s, _)| *s).collect::<Vec<_>>();
    assert_eq!(sids(&host), vec![1], "the second waits its turn");
    plugin.cold.last_start = Some(Instant::now() - STAGGER - Duration::from_secs(1));
    plugin.cold_restore(&host, &scan, new_shell);
    plugin.cold.last_start = Some(Instant::now() - STAGGER - Duration::from_secs(1));
    plugin.cold_restore(&host, &scan, new_shell);
    assert_eq!(sids(&host), vec![1, 4]);
}

/// The account it ran on is out: the allocator places it, and the
/// line names that account.
#[test]
fn a_restore_moves_only_when_its_account_cannot_take_it() {
    let home = quota_home("coldmove");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(77, &home, 1)]);
    let mut scan = unbound_scan(&[77]);
    scan.usage = marspot::cc_usage::read();
    assert!(scan.usage.is_some(), "the feed is read");
    plugin.cold_restore(&host, &scan, new_shell);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.contains(".claude-profile-2"), "{}", sent[0].1);
    let logged = host.logged.lock().unwrap().clone();
    assert!(logged.iter().any(|(e, l)| e == "cold_restore.start" && l.contains("on P2 (the account it ran on cannot take it)")), "{logged:?}");

    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(78, &home, 2)]);
    let mut scan = unbound_scan(&[78]);
    scan.usage = marspot::cc_usage::read();
    plugin.cold_restore(&host, &scan, new_shell);
    assert!(host.sent.lock().unwrap()[0].1.contains(".claude-profile-2"), "an account with room keeps it");
}

/// A restore that does not bring claude back is not tried again by
/// the next process.
#[test]
fn a_failed_restore_forgets_the_pane() {
    let home = TestHome::new("cold-failed");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(9, &home, 1), row(10, &home, 1)]);
    let ends = Arc::clone(&plugin.cold.ends);
    ends.lock().unwrap().push((9, pty_op::OpOutcome::TimedOut { step: 3, label: "await_process" }));
    ends.lock().unwrap().push((10, pty_op::OpOutcome::Done));
    plugin.settle_cold_ends(&host);
    assert_eq!(plugin.cold.rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![10]);
    let saved = fs::read_to_string(home.join("state").join("last_bound.tsv")).unwrap();
    assert!(!saved.contains("u-9\t"), "{saved}");
}

/// The person typed into the pane after its new shell started: a half
/// typed command is theirs, and the row goes.  Another pane is fine.
#[test]
fn a_pane_typed_into_since_its_shell_started_is_left_alone() {
    let home = TestHome::new("cold-typed");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(5, &home, 1)]);
    let typed = |_: u64| Some(PaneNow { shell: young_shell(), idle_shell: true, last_input: Some(SystemTime::now()) });
    plugin.cold_restore(&host, &unbound_scan(&[5]), typed);
    assert!(host.sent.lock().unwrap().is_empty());
    let before = |_: u64| Some(PaneNow { shell: young_shell(), idle_shell: true, last_input: Some(SystemTime::now() - Duration::from_secs(60)) });
    plugin.cold_restore(&host, &unbound_scan(&[5]), before);
    assert_eq!(host.sent.lock().unwrap().len(), 1, "input before the shell started is not the pane's new work");
}

/// A session already running in another pane, or named by two rows, is
/// resumed in one pane at most.
#[test]
fn a_session_bound_elsewhere_is_not_resumed_again() {
    let home = TestHome::new("cold-dup");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = plugin_with(vec![row(1, &home, 1)]);
    let mut scan = unbound_scan(&[1, 2]);
    scan.new_meta.insert(2, pane_on_p1(&home, "u-1"));
    plugin.cold_restore(&host, &scan, new_shell);
    assert!(host.sent.lock().unwrap().is_empty(), "u-1 runs in pane 2");

    let host = FakeHost::new(home.join("state"));
    let twin = LastBound { uuid: "u-1".into(), ..row(3, &home, 1) };
    let mut plugin = plugin_with(vec![row(1, &home, 1), twin]);
    let scan = unbound_scan(&[1, 3]);
    plugin.cold_restore(&host, &scan, new_shell);
    plugin.cold.last_start = None;
    plugin.cold_restore(&host, &scan, new_shell);
    let sids: Vec<u64> = host.sent.lock().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(sids, vec![1], "the same session is not resumed in pane 3 too");
}
