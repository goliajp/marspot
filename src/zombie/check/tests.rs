use std::path::PathBuf;

use crate::pidtree::{ProcRow, ProcTable};

use super::super::records::{Rec, ToolUse};
use super::super::rules::HOUR;
use super::super::tests::{MapProbe, helper, row};
use super::super::transcript::{ClaudeLog, SubLog, SubMeta};
use super::*;

const NOW: u64 = 1_800_000_000;
const OUT: &str = "/private/tmp/claude-501/-w-p/32f6eecf-5377-400d-958d-1ca24f1821d7/tasks/";

/// claude (pid 10) in pane 7 with tasks started from `cmds`, each a root
/// `zsh -c` with a script under it, silent nine hours.
fn pane_with(cmds: &[&str]) -> (ProcTable, MapProbe, ClaudeLog) {
    let mut rows = vec![row(1, 0, 1, 0, "launchd"), row(2, 1, 2, 0, "zsh"), row(10, 2, 10, 100, "claude"), row(11, 10, 10, 100, "smix-mcp")];
    let mut probe = MapProbe::default();
    let mut log = ClaudeLog::default();
    for (i, cmd) in cmds.iter().enumerate() {
        let (root, script) = (100 + i as i32 * 2, 101 + i as i32 * 2);
        let id = format!("b{i:08}");
        rows.push(row(root, 10, root, 1000 + i as u64, "zsh"));
        rows.push(row(script, root, root, 1000 + i as u64, "bash"));
        let out = PathBuf::from(format!("{OUT}{id}.output"));
        probe.stdout.insert(root, out.clone());
        probe.written.insert(out, NOW - 9 * HOUR);
        probe.argv.insert(root, vec!["/bin/zsh".into(), "-c".into(), cmd.to_string()]);
        probe.argv.insert(script, cmd.split(' ').map(str::to_string).collect());
        let call = ToolUse { id: format!("c{i}"), name: "Bash".into(), background: true, command: Some(cmd.to_string()), timeout_ms: None };
        log.main.feed(&Rec::Assistant { at: 1000, uses: vec![call], end: false }, &mut log.tasks);
        log.main.feed(&Rec::User { at: 1000, results: vec![(format!("c{i}"), Some(id))], notes: vec![] }, &mut log.tasks);
    }
    (ProcTable::from_rows(rows), probe, log)
}

fn scene<'a>(t: &'a ProcTable, log: &'a ClaudeLog, now: u64) -> Scene<'a> {
    Scene {
        now,
        table: t,
        panes: vec![PaneIn { sid: 7, agent: t.get(10).unwrap(), log }],
        outside: Vec::new(),
        uid: 501,
        tmpdir: None,
        helper: &helper,
    }
}

fn keys(h: &[Hit]) -> Vec<(Rule, String)> {
    h.iter().map(|h| (h.key.rule, h.key.id.clone())).collect()
}

/// Two checks half an hour apart with `cpu_ns` per root and script in between.
fn two_checks(t: &ProcTable, probe: &mut MapProbe, log: &ClaudeLog, cpu_ns: u64) -> Vec<Hit> {
    let mut w = Watch::default();
    for pid in t.rows().iter().map(|r| r.pid) {
        probe.cpu.insert(pid, 0);
    }
    assert!(w.hits(&scene(t, log, NOW - 1800), probe).is_empty());
    for pid in t.rows().iter().map(|r| r.pid) {
        probe.cpu.insert(pid, cpu_ns);
    }
    w.hits(&scene(t, log, NOW), probe)
}

#[test]
fn zombie_check_b1_names_the_older_copy_by_its_root_process() {
    let (t, mut probe, log) = pane_with(&["bash w.sh --stopped 3600", "bash w.sh --stopped 3600"]);
    let hits = two_checks(&t, &mut probe, &log, 100_000_000);
    assert_eq!(keys(&hits), vec![(Rule::B1, "100@1000000000".into())]);
    assert!(hits[0].evidence.contains("b00000001"), "names the newer copy: {}", hits[0].evidence);
}

#[test]
fn zombie_check_cpu_counts_the_whole_subtree_with_its_reaped_children() {
    // 0.3 s on the root and 0.3 s on its script: 0.6 s together, over the limit
    let (t, mut probe, log) = pane_with(&["bash w.sh", "bash w.sh"]);
    assert!(two_checks(&t, &mut probe, &log, 300_000_000).is_empty());
}

#[test]
fn zombie_check_leaves_the_measurement_lock_alone() {
    let (t, mut probe, log) = pane_with(&["bench-lock heavy cargo test", "bench-lock heavy cargo test"]);
    assert!(two_checks(&t, &mut probe, &log, 0).is_empty());
}

#[test]
fn zombie_check_leaves_a_task_with_marspot_below_it_to_the_rules() {
    // only the measurement lock and caffeinate shield the task above them
    let (t0, mut probe, log) = pane_with(&["bash w.sh", "bash w.sh"]);
    let mut rows = t0.rows().to_vec();
    rows.push(row(150, 101, 100, 1000, "marspot-peer"));
    let t = ProcTable::from_rows(rows);
    probe.argv.insert(150, vec!["marspot-peer".into(), "pane".into()]);
    probe.cpu.insert(150, 0);
    assert_eq!(keys(&two_checks(&t, &mut probe, &log, 0)), vec![(Rule::B1, "100@1000000000".into())]);
}

#[test]
fn zombie_check_cuts_a_long_evidence_to_two_kib() {
    let long = format!("bash w.sh {}", "长".repeat(2000));
    let (t, mut probe, log) = pane_with(&[&long, &long]);
    let hits = two_checks(&t, &mut probe, &log, 0);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].evidence.len() <= EVIDENCE && hits[0].evidence.ends_with('…'), "{}", hits[0].evidence.len());
}

#[test]
fn zombie_check_takes_a_leaf_that_changes_every_minute_as_one_task() {
    let (t, mut probe, log) = pane_with(&["bash w.sh", "bash w.sh"]);
    let mut w = Watch::default();
    for pid in [100, 101, 102, 103] {
        probe.cpu.insert(pid, 0);
    }
    w.hits(&scene(&t, &log, NOW - 1800), &mut probe);
    let mut rows = t.rows().to_vec();
    rows.push(row(150, 101, 100, NOW - 30, "sleep"));
    let t2 = ProcTable::from_rows(rows);
    let hits = w.hits(&scene(&t2, &log, NOW), &mut probe);
    assert_eq!(keys(&hits), vec![(Rule::B1, "100@1000000000".into())], "keyed by the root, not the sleep");
}

#[test]
fn zombie_check_b3_names_a_task_still_running_after_its_end_was_told() {
    let (t, mut probe, mut log) = pane_with(&["bash a.sh"]);
    let note = super::super::records::Note { task: "b00000000".into(), ended: true, waiting: false };
    log.main.feed(&Rec::Queued { at: NOW - 31 * 60, notes: vec![note] }, &mut log.tasks);
    let hits = Watch::default().hits(&scene(&t, &log, NOW), &mut probe);
    assert_eq!(keys(&hits), vec![(Rule::B3, "100@1000000000".into())]);
}

#[test]
fn zombie_check_s1_names_a_subagent_by_its_id_and_holds_it_for_ten_minutes() {
    let (t, mut probe, mut log) = pane_with(&[]);
    let mut sub = SubLog::born(Some(500));
    sub.feed(&Rec::Assistant { at: NOW - HOUR, uses: vec![], end: true }, &mut log.tasks);
    let meta = SubMeta { description: "Step 3b".into(), depth: 2, parent: Some("aaf0".into()), finished: false };
    log.subs.insert("a1".into(), (sub, meta));
    log.main.feed(&Rec::TurnEnd { at: NOW - HOUR }, &mut log.tasks);
    let mut w = Watch::default();
    assert!(w.check(&scene(&t, &log, NOW), &mut probe).is_empty(), "found once is not yet a suspect");
    assert!(w.check(&scene(&t, &log, NOW + STABLE - 1), &mut probe).is_empty());
    let s = w.check(&scene(&t, &log, NOW + STABLE), &mut probe);
    assert_eq!(s.len(), 1);
    assert_eq!((s[0].hit.key.rule, s[0].hit.key.id.as_str(), s[0].since), (Rule::S1, "agent:a1", NOW));
    assert!(s[0].hit.evidence.contains("Step 3b") && s[0].hit.evidence.contains("TaskStop a1"));
}

#[test]
fn zombie_check_s1_leaves_a_subagent_waiting_on_a_subagent_it_started() {
    // a subagent whose turn is over while a subagent it started still works
    let (t, mut probe, mut log) = pane_with(&[]);
    let mut sub = SubLog::born(Some(500));
    sub.feed(&Rec::Assistant { at: NOW - HOUR, uses: vec![], end: true }, &mut log.tasks);
    log.subs.insert("exec".into(), (sub, SubMeta::default()));
    let mut worker = SubLog::born(Some(600));
    worker.feed(&Rec::Assistant { at: NOW - 60, uses: vec![], end: false }, &mut log.tasks);
    log.subs.insert("w1".into(), (worker, SubMeta { parent: Some("exec".into()), ..SubMeta::default() }));
    log.main.feed(&Rec::TurnEnd { at: NOW - HOUR }, &mut log.tasks);
    assert!(Watch::default().hits(&scene(&t, &log, NOW), &mut probe).is_empty());
    log.subs.remove("w1");
    assert_eq!(keys(&Watch::default().hits(&scene(&t, &log, NOW), &mut probe)), vec![(Rule::S1, "agent:exec".into())]);
}

#[test]
fn zombie_check_forgets_a_hit_missing_from_one_check() {
    let (t, mut probe, mut log) = pane_with(&[]);
    let mut sub = SubLog::born(Some(500));
    sub.feed(&Rec::Assistant { at: NOW - HOUR, uses: vec![], end: true }, &mut log.tasks);
    log.subs.insert("a1".into(), (sub, SubMeta::default()));
    log.main.feed(&Rec::TurnEnd { at: NOW - HOUR }, &mut log.tasks);
    let mut busy = log.clone();
    busy.main.turn_ended = None;
    let mut w = Watch::default();
    w.check(&scene(&t, &log, NOW), &mut probe);
    w.check(&scene(&t, &busy, NOW + 300), &mut probe);
    assert!(w.check(&scene(&t, &log, NOW + STABLE), &mut probe).is_empty(), "the run of checks started over");
}

fn outside_scene<'a>(t: &'a ProcTable, log: &'a ClaudeLog, r: &'a ProcRow, gone: bool) -> Scene<'a> {
    let mut s = scene(t, log, NOW);
    s.outside.push(OutsideIn { sid: 7, row: r, group: Vec::new(), session_gone: gone, why: "令牌".into() });
    s
}

#[test]
fn zombie_check_o2_names_a_quiet_server_left_in_a_project_tmp_and_not_a_bundle_or_a_worktree() {
    let (mut rows, mut probe, log) = {
        let (t, p, l) = pane_with(&[]);
        (t.rows().to_vec(), p, l)
    };
    rows.push(row(300, 1, 300, NOW - 2 * 24 * HOUR, "node"));
    let t = ProcTable::from_rows(rows);
    let r = t.get(300).unwrap();
    let tmp = PathBuf::from("/w/proj").join(".tmp");
    probe.cwd.insert(300, tmp.join("vite"));
    probe.cpu.insert(300, 0);
    let mut w = Watch::default();
    let mut early = outside_scene(&t, &log, r, true);
    early.now = NOW - 3600;
    assert!(w.hits(&early, &mut probe).is_empty(), "no hour of CPU yet");
    assert_eq!(keys(&w.hits(&outside_scene(&t, &log, r, true), &mut probe)), vec![(Rule::O2, format!("300@{}", r.start_us))]);
    assert!(w.hits(&outside_scene(&t, &log, r, false), &mut probe).is_empty(), "its session still runs");
    probe.exe.insert(300, PathBuf::from("/Applications/X.app/Contents/MacOS/X"));
    assert!(w.hits(&outside_scene(&t, &log, r, true), &mut probe).is_empty(), "an app's own process");
    probe.exe.insert(300, PathBuf::from("/Applications/Xcode.app/Contents/Developer/usr/bin/simctl"));
    assert_eq!(w.hits(&outside_scene(&t, &log, r, true), &mut probe).len(), 1, "a developer tool is not excluded");
    probe.exe.remove(&300);
    probe.cwd.insert(300, tmp.join("worktrees").join("a"));
    assert!(w.hits(&outside_scene(&t, &log, r, true), &mut probe).is_empty(), "a worktree");
}

#[test]
fn zombie_check_never_names_codex_servers_or_marspot() {
    let (t0, mut probe, log) = pane_with(&[]);
    let mut rows = t0.rows().to_vec();
    rows.push(row(400, 1, 400, 0, "codex"));
    rows.push(row(401, 1, 401, 0, "marspot-peer"));
    let t = ProcTable::from_rows(rows);
    probe.argv.insert(400, vec!["codex".into(), "app-server".into(), "daemon".into()]);
    for pid in [400, 401] {
        probe.cwd.insert(pid, PathBuf::from("/w").join(".tmp"));
        probe.cpu.insert(pid, 0);
    }
    let mut w = Watch::default();
    for pid in [400, 401] {
        let r = t.get(pid).unwrap();
        let mut s = outside_scene(&t, &log, r, true);
        s.now = NOW - 3600;
        w.hits(&s, &mut probe);
        assert!(w.hits(&outside_scene(&t, &log, r, true), &mut probe).is_empty(), "pid {pid}");
    }
}
