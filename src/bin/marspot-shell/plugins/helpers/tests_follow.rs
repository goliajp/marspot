//! What a start and an end may remove, and what is told again.

use std::collections::HashSet;
use std::path::Path;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started, temp};
use super::turn::{self, Turn};
use super::work::{self, Job};
use super::{notify, rebuild, records, tick};

fn begun(agent: AgentKind) -> Vec<Event> {
    vec![Event::Started(started(10, "shell", agent)), Event::TaskDelivered]
}

fn make(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("task.md"), "x").unwrap();
}

/// Append to the log what this build cannot read.
fn garble(log: &Path, bytes: &[u8]) {
    let mut b = std::fs::read(log).unwrap();
    b.extend_from_slice(bytes);
    std::fs::write(log, b).unwrap();
}

#[test]
fn helpers_a_start_keeps_the_directory_of_a_running_one_its_log_lacks() {
    let dir = temp("lacks");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, begun(AgentKind::Codex));
    // a later build's helper, and half a line that is not UTF-8
    garble(&s.log, b"1\t1\t2\tstarted\tparent=10\tcaller=shell\tagent=other\tbudget=30\n");
    garble(&s.log, b"1\t1\t1\tinput_seen\tx=\xff\xfe");
    feed(&s, 3, begun(AgentKind::Codex));
    feed(&s, 4, [begun(AgentKind::Codex), vec![Event::Stopped]].concat());
    for sid in 1..=5 {
        make(&s.dir_of(sid));
    }
    assert_eq!(rebuild::rebuild(&s, || (1..=4).collect()), 2, "a bad line loses itself alone");
    let left: Vec<bool> = (1..=5).map(|sid| s.dir_of(sid).exists()).collect();
    assert_eq!(left, [true, true, true, false, false], "2 runs, 4 stopped, 5 is gone");
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_start_that_cannot_read_the_log_removes_nothing() {
    let dir = temp("unreadable");
    let (s, _rx) = shared_at(&dir);
    std::fs::create_dir(&s.log).unwrap();
    make(&s.dir_of(1));
    assert!(records::replay(&s.log).is_err());
    assert_eq!(rebuild::rebuild(&s, HashSet::new), 0);
    assert!(s.dir_of(1).exists());
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_an_end_removes_only_what_stopped_or_is_gone() {
    let dir = temp("only");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, [begun(AgentKind::Codex), vec![Event::Stopped]].concat());
    feed(&s, 2, begun(AgentKind::Codex));
    for sid in 1..=4 {
        make(&s.dir_of(sid));
    }
    // 3 runs although no table has it, 2's session is missed by the walk
    s.sweep_dirs_where(|sid| sid == 2 || sid == 4);
    let left: Vec<bool> = (1..=4).map(|sid| s.dir_of(sid).exists()).collect();
    assert_eq!(left, [false, true, true, false]);
    assert!(s.lock().stopped.contains(&1), "its agent may still write");
    make(&s.dir_of(1));
    s.sweep_dirs_where(|_| false);
    assert!(!s.dir_of(1).exists());
    s.sweep_dirs_where(|sid| sid == 1);
    assert!(s.lock().stopped.is_empty(), "nothing to remember");
    // one that is not there is no failure
    super::dirs::remove(&s.root(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_one_still_at_its_approval_prompt_is_not_in_a_turn() {
    let dir = temp("pokedask");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, [begun(AgentKind::Kimi), vec![Event::BlockedNoticed { prompt: String::new() }, Event::InputSeen]].concat());
    assert!(s.lock().poked.contains_key(&1));
    let look = |kind| {
        let host = host_with(&[(1, PaneStatus::Busy(kind))]);
        turn::turn(&s, &host, &s.table(), &s.table().map[&1], records::now_ms(), &mut Default::default());
        s.lock().poked.contains_key(&1)
    };
    assert!(look(BusyKind::ToolAwaitingApproval), "the keys answered nothing");
    assert!(!look(BusyKind::ToolExecuting));
    feed(&s, 1, vec![Event::IdleNoticed, Event::InputSeen]);
    assert!(!look(BusyKind::Working));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_one_with_a_stop_held_back_was_not_merely_poked() {
    let dir = temp("pokedheld");
    let (s, _rx) = shared_at(&dir);
    let told = [begun(AgentKind::Claude), vec![Event::IdleNoticed, Event::InputSeen]].concat();
    feed(&s, 1, told.clone());
    feed(&s, 2, [told, vec![Event::NoticeSuppressed { blocked: false }]].concat());
    let poked: Vec<u64> = turn::poked_at_start(&s.table(), 1).into_keys().collect();
    assert_eq!(poked, [1]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_another_approval_prompt_after_a_start_is_told() {
    let dir = temp("another");
    let (s, rx) = shared_at(&dir);
    let at = |screen: &str| vec![Event::BlockedNoticed { prompt: notify::prompt_sha(screen) }];
    feed(&s, 1, [begun(AgentKind::Claude), at("Run make?\n❯ 1. Yes\n  2. No")].concat());
    feed(&s, 2, [begun(AgentKind::Claude), vec![Event::BlockedNoticed { prompt: String::new() }]].concat());
    let carried = Event::Carried(Box::new(s.table().map[&1].clone()));
    assert_eq!(records::parse(&records::line(1, 1, 1, &carried)), Some((1, 1, carried)));
    assert_eq!(records::replay(&s.log).unwrap().table, *s.table());

    // the first look after a start asks which prompt it is, once
    let mut in_turn = turn::turns_at_start(&s.table());
    let host = host_with(&[(1, PaneStatus::Busy(BusyKind::ToolAwaitingApproval))]);
    let mut asked = || {
        tick::tick(&s, &host, &mut in_turn, &mut tick::Parked::default());
        rx.try_iter().filter(|j| matches!(j, Job::StillBlocked { sid: 1 })).count()
    };
    assert_eq!((asked(), asked()), (1, 0));
    assert_eq!(in_turn[&1], Turn::BlockedTold);

    let told = |sid: u64, screen: Option<&str>| {
        work::still_blocked(&s, sid, screen.map(String::from));
        // no pane is there to take it: the notice is kept
        let h: Helper = s.table().map[&sid].clone();
        s.lock().undelivered.take(sid).is_some() || h.suppressed > 0
    };
    assert!(!told(1, Some("Run make?\n  1. Yes\n❯ 2. No")), "the cursor moved");
    assert!(!told(1, None), "not seen");
    assert!(!told(2, Some("Run make?")), "what was told is not known");
    assert!(told(1, Some("Run make clean?\n❯ 1. Yes\n  2. No")));
    std::fs::remove_dir_all(&dir).unwrap();
}
