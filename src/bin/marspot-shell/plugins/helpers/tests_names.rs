use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use marspot::pidtree::{ProcId, proc_id};
use marspot_peer::control::AgentKind;

use super::on_change;
use crate::plugins::helpers::pair::{Move, Moved, note_move, write_moves};
use crate::plugins::helpers::table::{Event, Started};
use crate::plugins::helpers::tests::{feed, shared_at, started, temp};
use crate::plugins::helpers::work::Job;
use crate::plugins::helpers::{Shared, timers};

/// Helper 10 of pane 1, which was handed to `heir` in a new conversation;
/// conversation records are in `sessions`.
fn waiting(s: &Arc<Shared>, sessions: &std::path::Path, heir: ProcId) {
    std::fs::create_dir_all(sessions).unwrap();
    s.lock().names.dirs = Some(vec![sessions.to_path_buf()]);
    feed(s, 10, vec![Event::Started(Started { conv: Some("one".into()), ..started(1, "claude", AgentKind::Codex) })]);
    let left = ProcId { pid: 50, start_us: 5 };
    note_move(s, Move { m: Moved { pane: 1, from: Some("one".into()), left, resumed: None }, to: heir });
    write_moves(s);
}

/// The wakes queued since the last call.
fn wakes(s: &Shared, rx: &Receiver<Job>) -> usize {
    let n = rx.try_iter().filter(|j| matches!(j, Job::Wake)).count();
    s.wake_queued.store(false, Ordering::Release);
    n
}

fn within(done: impl Fn() -> bool) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// The record claude writes for its session `id`, as process `pid`.
fn claude_record(sessions: &std::path::Path, pid: i32, id: &str) {
    let start = marspot_peer::proc::lstart(pid).unwrap();
    let text = format!(r#"{{"pid":{pid},"sessionId":"{id}","procStart":"{start}","entrypoint":"cli","kind":"interactive"}}"#);
    let tmp = sessions.join(format!(".{pid}.json.tmp"));
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, sessions.join(format!("{pid}.json"))).unwrap();
}

#[test]
fn helpers_waiting_for_a_name_wakes_on_the_agents_record_and_on_nothing_else() {
    let dir = temp("namewatch");
    let (s, rx) = shared_at(&dir);
    let me = proc_id(std::process::id() as i32).unwrap();
    let sessions = dir.join("sessions");
    waiting(&s, &sessions, me);
    on_change(&s);
    assert!(s.lock().names.unnamed.contains(&1), "no record yet");
    assert!(s.lock().names.watching());
    wakes(&s, &rx);

    // nothing changes: no wake, no clock, no look
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(wakes(&s, &rx), 0, "no wake with nothing changed");
    assert_eq!(timers::sleep_for(&s), None, "no clock");
    // another agent's record, and the directory's own: not the heir's
    claude_record(&sessions, std::os::unix::process::parent_id() as i32, "other");
    std::fs::write(dir.join("sessions").join("77.json"), "{}").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(wakes(&s, &rx), 0, "no wake for another agent's record");

    // the agent names its conversation
    claude_record(&sessions, me.pid, "two");
    assert!(within(|| rx.try_iter().any(|j| matches!(j, Job::Wake))), "woken by the record");
    on_change(&s);
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("two"));
    assert!(s.lock().names.unnamed.is_empty() && !s.lock().names.watching(), "and no longer watched");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// claude writes its record as it starts, often before the move is
/// written down: the first look reads it, from the agent's own file,
/// with no event after.
#[test]
fn helpers_waiting_for_a_name_find_a_record_written_before_they_wait() {
    let dir = temp("nameearly");
    let (s, rx) = shared_at(&dir);
    let me = proc_id(std::process::id() as i32).unwrap();
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    claude_record(&sessions, me.pid, "two");
    waiting(&s, &sessions, me);
    on_change(&s);
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("two"), "paired at the first look");
    assert!(s.lock().names.unnamed.is_empty() && !s.lock().names.watching());
    wakes(&s, &rx);
    // the /clear that follows is not followed
    claude_record(&sessions, me.pid, "after-clear");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(wakes(&s, &rx), 0);
    on_change(&s);
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("two"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_waiting_for_a_name_end_when_the_agent_exits() {
    let dir = temp("nameexit");
    let (s, rx) = shared_at(&dir);
    let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let heir = proc_id(child.id() as i32).unwrap();
    waiting(&s, &dir.join("sessions"), heir);
    on_change(&s);
    assert!(s.lock().names.watching());
    wakes(&s, &rx);

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(within(|| rx.try_iter().any(|j| matches!(j, Job::Wake))), "woken by the exit");
    on_change(&s);
    assert!(s.lock().names.unnamed.is_empty() && !s.lock().names.watching());
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("one"), "follows none");
    std::fs::remove_dir_all(&dir).unwrap();
}
