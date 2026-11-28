//! Putting codex back in panes whose session process started afresh.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use marspot::pidtree::{ProcId, ProcTable};
use marspot_term::test_env::{EnvGuard, TempDir};

use super::super::cold_record::LastThread;
use super::super::CodexPlugin;
use super::{Bound, Cold};
use crate::observe::{Observation, Session};
use crate::plugins::cold_restore::{Pace, PaneNow, STAGGER};
use crate::plugins::l3_test_support::OpsHost;
use crate::plugins::pty_op;

#[path = "cold_restore_l3.rs"]
mod cold_restore_l3;

fn thread(sid: u64) -> String {
    format!("01a133f4-8e7b-7331-9630-{sid:012}")
}

fn row(sid: u64) -> LastThread {
    LastThread {
        sid,
        thread: thread(sid),
        home: "/Users/x/.codex-profile-2".into(),
        cwd: Some("/w/proj".into()),
        model: Some("gpt-5.5".into()),
        shell: ProcId { pid: 1, start_us: 1 },
    }
}

fn session(sid: u64, shell: Option<ProcId>) -> Session {
    let mut s = crate::observe::tests::session_with(Vec::new(), 0);
    s.entry.id = sid;
    s.shell = shell;
    s
}

fn obs(sids: &[u64]) -> Observation {
    let sessions = sids.iter().map(|sid| session(*sid, Some(young_shell()))).collect();
    Observation { at: Instant::now(), table: ProcTable::from_rows(Vec::new()), sessions }
}

/// A shell started a moment ago, so inside the window.
fn young_shell() -> ProcId {
    let us = (SystemTime::now() - Duration::from_secs(5)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_micros() as u64;
    ProcId { pid: 4242, start_us: us }
}

fn new_shell(_: u64) -> Option<PaneNow> {
    Some(PaneNow { shell: young_shell(), idle_shell: true, last_input: None })
}

fn plugin_with(rows: Vec<LastThread>) -> CodexPlugin {
    let mut plugin = CodexPlugin::new();
    plugin.cold = Cold { rows, pace: Pace::own(), ..Cold::default() };
    plugin
}

fn host(dir: &TempDir) -> OpsHost {
    OpsHost::new(dir.join("state"), Arc::new(crate::plugins::l3_test_support::NoPane))
}

fn sids(h: &OpsHost) -> Vec<u64> {
    h.sent.lock().unwrap().iter().map(|(s, _)| *s).collect()
}

/// After the restart a codex pane sits at an empty shell; its thread
/// comes back under the home it ran under, on its model, in its
/// directory, with nothing interrupted first.
#[test]
fn a_new_shell_gets_its_codex_thread_back_under_the_home_it_ran_on() {
    let dir = TempDir::new("codex-cold-same");
    let host = host(&dir);
    let mut plugin = plugin_with(vec![row(483)]);
    let none = HashSet::new();
    plugin.cold_restore(&host, &obs(&[483]), &[], &none, new_shell);
    let sent = host.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let line = &sent[0].1;
    assert!(!line.contains('\x03'), "nothing is interrupted first: {line:?}");
    for want in ["CODEX_HOME='/Users/x/.codex-profile-2'", &format!("codex resume {} -m gpt-5.5", thread(483)), "( cd '/w/proj' && "] {
        assert!(line.contains(want), "{want} missing from {line}");
    }
    let labels = host.submitted.lock().unwrap()[0].1.clone();
    let at = |l: &str| labels.iter().position(|x| *x == l).unwrap_or_else(|| panic!("{l} in {labels:?}"));
    assert_eq!(at("untouched") + 1, at("resume"), "checked right before the line: {labels:?}");
    assert!(at("resume") < at("first_frame"), "{labels:?}");
    let logged = host.logged.lock().unwrap().clone();
    let start = logged.iter().find(|(e, _)| e == "cold_restore.start").expect("a start line");
    assert!(start.1.contains("pane 483") && start.1.contains("on P2 (the account it ran on)"), "{}", start.1);
    plugin.cold.pace.set_last_start(None);
    plugin.cold_restore(&host, &obs(&[483]), &[], &none, new_shell);
    assert_eq!(host.sent.lock().unwrap().len(), 1, "once per pane in one process");
}

/// One restore per `STAGGER`; a pane running codex, a pane whose thread
/// is open elsewhere, and a pane mid-switch owe nothing.
#[test]
fn restores_are_staggered_and_skip_panes_with_codex_or_a_thread_open_elsewhere() {
    let dir = TempDir::new("codex-cold-stagger");
    let host = host(&dir);
    let mut plugin = plugin_with((1..=4).map(row).collect());
    let with_codex: HashSet<u64> = [3].into();
    // pane 9 runs pane 2's thread now
    let bound = [Bound { sid: 9, thread: thread(2), home: "/Users/x/.codex-profile-2".into(), cwd: None, model: None }];
    let o = obs(&[1, 2, 3, 4, 9]);
    plugin.cold_restore(&host, &o, &bound, &with_codex, new_shell);
    plugin.cold_restore(&host, &o, &bound, &with_codex, new_shell);
    assert_eq!(sids(&host), vec![1], "the second waits its turn");
    for _ in 0..2 {
        plugin.cold.pace.set_last_start(Some(Instant::now() - STAGGER - Duration::from_secs(1)));
        plugin.cold_restore(&host, &o, &bound, &with_codex, new_shell);
    }
    assert_eq!(sids(&host), vec![1, 4]);
}

/// A thread named by two rows is resumed in one pane at most, and a
/// pane whose session process is not running is waited for.
#[test]
fn a_thread_is_resumed_once_and_a_pane_not_back_yet_waits() {
    let dir = TempDir::new("codex-cold-dup");
    let host = host(&dir);
    let twin = LastThread { thread: thread(1), ..row(3) };
    let mut plugin = plugin_with(vec![row(1), twin, row(5)]);
    let mut o = obs(&[1, 3, 5]);
    o.sessions[2].live = false;
    let none = HashSet::new();
    for _ in 0..3 {
        plugin.cold_restore(&host, &o, &[], &none, new_shell);
        plugin.cold.pace.set_last_start(None);
    }
    assert_eq!(sids(&host), vec![1], "the same thread is not resumed in pane 3, pane 5 is not back");
}

/// The person typed into the pane after its new shell started: a half
/// typed command is theirs.
#[test]
fn a_pane_typed_into_since_its_shell_started_is_left_alone() {
    let dir = TempDir::new("codex-cold-typed");
    let host = host(&dir);
    let mut plugin = plugin_with(vec![row(5)]);
    let none = HashSet::new();
    let typed = |_: u64| Some(PaneNow { shell: young_shell(), idle_shell: true, last_input: Some(SystemTime::now()) });
    plugin.cold_restore(&host, &obs(&[5]), &[], &none, typed);
    assert!(host.sent.lock().unwrap().is_empty());
    let busy = |_: u64| Some(PaneNow { shell: young_shell(), idle_shell: false, last_input: None });
    plugin.cold_restore(&host, &obs(&[5]), &[], &none, busy);
    assert!(host.sent.lock().unwrap().is_empty(), "a program in front");
}

/// A codex pane's thread goes to disk when it is known, and comes back
/// in the next process.
#[test]
fn a_bound_thread_is_written_and_read_back() {
    let dir = TempDir::new("codex-cold-note");
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", dir.join("state"));
    std::fs::create_dir_all(marspot_term::session_registry::session_dir(7)).unwrap();
    let host = host(&dir);
    let mut plugin = plugin_with(Vec::new());
    let bound = [Bound { sid: 7, thread: thread(7), home: "/Users/x/.codex-profile-3".into(), cwd: Some("/w".into()), model: Some("gpt-5.5".into()) }];
    let with_codex: HashSet<u64> = [7].into();
    plugin.note_threads(&host, &obs(&[7]), &bound, &with_codex);
    let saved = std::fs::read_to_string(dir.join("state").join("last_bound.tsv")).unwrap();
    assert!(saved.starts_with(&format!("7\t{}\t4242\t", thread(7))) && saved.contains("/Users/x/.codex-profile-3\t/w\tgpt-5.5"), "{saved}");
    let mut next = CodexPlugin::new();
    next.load_last_bound(&host);
    assert_eq!(next.cold.rows, plugin.cold.rows);

    // the app closed the pane: its row goes
    marspot_term::session_registry::mark_closed(7).unwrap();
    plugin.note_threads(&host, &obs(&[7]), &[], &HashSet::new());
    assert!(plugin.cold.rows.is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("state").join("last_bound.tsv")).unwrap(), "");
}

/// A restore that does not bring codex back is not tried again by the
/// next process.
#[test]
fn a_failed_restore_forgets_the_pane() {
    let dir = TempDir::new("codex-cold-failed");
    let host = host(&dir);
    let mut plugin = plugin_with(vec![row(9), row(10)]);
    let ends = Arc::clone(&plugin.cold.ends);
    ends.lock().unwrap().push((9, pty_op::OpOutcome::TimedOut { step: 5, label: "await_process" }));
    ends.lock().unwrap().push((10, pty_op::OpOutcome::Done));
    plugin.settle_cold_ends(&host);
    assert_eq!(plugin.cold.rows.iter().map(|r| r.sid).collect::<Vec<_>>(), vec![10]);
    let saved = std::fs::read_to_string(dir.join("state").join("last_bound.tsv")).unwrap();
    assert!(!saved.contains(&thread(9)), "{saved}");
}
