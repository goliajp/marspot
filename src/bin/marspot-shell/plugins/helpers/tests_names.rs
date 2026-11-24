use std::cell::Cell;
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
use crate::plugins::peerbus::Conversation;

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

#[test]
fn helpers_waiting_for_a_name_wakes_on_a_record_written_and_on_nothing_else() {
    let dir = temp("namewatch");
    let (s, rx) = shared_at(&dir);
    let me = proc_id(std::process::id() as i32).unwrap();
    waiting(&s, &dir.join("sessions"), me);
    let looks = Cell::new(0);
    let none = |_| {
        looks.set(looks.get() + 1);
        None
    };
    on_change(&s, none);
    assert_eq!(looks.get(), 1, "looked once when the move was written down");
    assert!(s.lock().names.watching());
    wakes(&s, &rx);

    // nothing changes: no wake, no clock, no look
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(wakes(&s, &rx), 0, "no wake with nothing changed");
    assert_eq!(timers::sleep_for(&s), None, "no clock");
    on_change(&s, none);
    assert_eq!(looks.get(), 1, "a job that is not about it does not look");

    // the agent names its conversation
    std::fs::write(dir.join("sessions").join("77.json"), "{}").unwrap();
    assert!(within(|| rx.try_iter().any(|j| matches!(j, Job::Wake))), "woken by the record");
    on_change(&s, |_| Some(Conversation { agent: me, id: "two".into() }));
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("two"));
    assert!(s.lock().names.unnamed.is_empty() && !s.lock().names.watching(), "and no longer watched");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_waiting_for_a_name_end_when_the_agent_exits() {
    let dir = temp("nameexit");
    let (s, rx) = shared_at(&dir);
    let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let heir = proc_id(child.id() as i32).unwrap();
    waiting(&s, &dir.join("sessions"), heir);
    on_change(&s, |_| None);
    assert!(s.lock().names.watching());
    wakes(&s, &rx);

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(within(|| rx.try_iter().any(|j| matches!(j, Job::Wake))), "woken by the exit");
    on_change(&s, |_| panic!("its agent is gone: nothing is asked"));
    assert!(s.lock().names.unnamed.is_empty() && !s.lock().names.watching());
    assert_eq!(s.table().map[&10].conv.as_deref(), Some("one"), "follows none");
    std::fs::remove_dir_all(&dir).unwrap();
}
