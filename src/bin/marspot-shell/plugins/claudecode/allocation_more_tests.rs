//! `boundary`, `compacted` and `reading` rows, each written once per
//! change, and every rest that lasts `BOUNDARY_GAP` with its row.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::claudecode::allocation_inputs::PaneIn;

const NOW: i64 = 1_795_716_000;

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-more-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn account(profile: u8, week: f64, measured_at: i64) -> AccountIn {
    AccountIn { profile, week_used: week, fable_used: None, fable_known: true, week_renews: NOW + 3600, fable_renews: NOW + 3600, open: true, measured_at }
}

fn pane(sid: u64, resting: bool, transcript: &Path) -> PaneIn {
    PaneIn { sid, profile: 3, class: Class::Plain, marked_at: None, refused_at: None, resting, helper: false, out: false, transcript: Some(transcript.to_path_buf()) }
}

fn snap(now: i64, panes: Vec<PaneIn>, accounts: Vec<AccountIn>) -> Snapshot {
    let live = panes.iter().map(|p| p.sid).collect();
    Snapshot { now, feed_at: Some(now), accounts, panes, live }
}

/// The rows of `kind` written so far: (at, detail).
fn rows(a: &Allocation, kind: &str) -> Vec<(i64, String)> {
    a.events.read_since(NOW - 10, NOW + 86_400).into_iter().filter(|e| e.kind == kind).map(|e| (e.at, e.detail)).collect()
}

/// Pane 481 rests, works 10 s and rests again: the second turn end is
/// inside the gap and gets its row once the gap is over, while it rests.
#[test]
fn allocation_boundary_one_row_per_turn_end_at_most_one_per_gap_and_none_lost() {
    let d = root("boundary");
    let t = d.join("s.jsonl");
    fs::write(&t, "").unwrap();
    let mut a = Allocation::new(d.join("alloc_events"));
    let holds: HashMap<u64, Hold> = [(481, Hold { child: true, focus: true, claude_pid: 7, ..Hold::default() })].into();
    let step = |a: &mut Allocation, at: i64, resting: bool| {
        a.record_more(&snap(at, vec![pane(481, resting, &t)], Vec::new()), &holds);
    };
    step(&mut a, NOW, false);
    step(&mut a, NOW + 2, true);
    step(&mut a, NOW + 4, true);
    step(&mut a, NOW + 10, false);
    step(&mut a, NOW + 20, true);
    assert_eq!(rows(&a, "boundary").len(), 1, "the second turn end is inside the gap");
    step(&mut a, NOW + 32, true);
    let got = rows(&a, "boundary");
    assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), [NOW + 2, NOW + 32]);
    assert_eq!(got[0].1, "holds=child,focus tasks=0 oldest_s=- helpers=0 child=1 timer=0 user=0 focus=1 tokens=-");
    // a turn end inside the gap whose rest is over by then has no row
    step(&mut a, NOW + 40, false);
    step(&mut a, NOW + 41, true);
    step(&mut a, NOW + 45, false);
    step(&mut a, NOW + 80, false);
    assert_eq!(rows(&a, "boundary").len(), 2);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_boundary_names_live_tasks_and_a_person_writing_and_a_compaction_is_a_row() {
    let d = root("tasks");
    let t = d.join("s.jsonl");
    let at = |s: i64| {
        let secs = NOW + s;
        let (h, m, sec) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
        format!("2026-11-26T{h:02}:{m:02}:{sec:02}.000Z")
    };
    let mut f = fs::File::create(&t).unwrap();
    writeln!(f, r#"{{"type":"user","timestamp":"{}","message":{{"content":"keep going"}}}}"#, at(-100)).unwrap();
    writeln!(f, r#"{{"type":"user","timestamp":"{}","message":{{"content":[{{"type":"tool_result","content":"ok"}}]}},"toolUseResult":{{"isAsync":true,"agentId":"a1"}}}}"#, at(-90)).unwrap();
    let mut a = Allocation::new(d.join("alloc_events"));
    let holds: HashMap<u64, Hold> = [(441, Hold { claude_pid: 9, ..Hold::default() })].into();
    a.record_more(&snap(NOW, vec![pane(441, true, &t)], Vec::new()), &holds);
    let got = rows(&a, "boundary");
    assert_eq!(got[0].1, "holds=tasks,user tasks=1 oldest_s=90 helpers=0 child=0 timer=0 user=1 focus=0 tokens=-");
    writeln!(f, r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{}","compactMetadata":{{"trigger":"manual","preTokens":257614}}}}"#, at(5)).unwrap();
    writeln!(f, r#"{{"type":"assistant","timestamp":"{}","message":{{"usage":{{"input_tokens":3,"cache_read_input_tokens":40000,"output_tokens":7}}}}}}"#, at(8)).unwrap();
    a.record_more(&snap(NOW + 10, vec![pane(441, true, &t)], Vec::new()), &holds);
    assert_eq!(rows(&a, "compacted"), [(NOW + 10, format!("before=257614 after=40010 trigger=manual at={}", NOW + 5))]);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_reading_written_when_it_changes_not_when_only_polled_again_and_known_after_a_restart() {
    let d = root("reading");
    let events = d.join("alloc_events");
    let mut a = Allocation::new(events.clone());
    let none = HashMap::new();
    a.record_more(&snap(NOW, Vec::new(), vec![account(3, 0.94, NOW), account(5, 0.09, NOW)]), &none);
    a.record_more(&snap(NOW + 300, Vec::new(), vec![account(3, 0.94, NOW + 300), account(5, 0.09, NOW + 300)]), &none);
    a.record_more(&snap(NOW + 600, Vec::new(), vec![account(3, 0.95, NOW + 600), account(5, 0.09, NOW + 600)]), &none);
    let mut b = Allocation::new(events.clone());
    b.record_more(&snap(NOW + 900, Vec::new(), vec![account(3, 0.95, NOW + 900), account(5, 0.09, NOW + 900)]), &none);
    let got: Vec<(i64, String)> = rows(&b, "reading");
    assert_eq!(got.iter().map(|r| r.0).collect::<Vec<_>>(), [NOW, NOW, NOW + 600]);
    assert_eq!(got[2].1, format!("week=0.95 fable=- week_renews={0} fable_renews={0} open=1 measured_at={1}", NOW + 3600, NOW + 600));
    fs::remove_dir_all(&d).unwrap();
}
