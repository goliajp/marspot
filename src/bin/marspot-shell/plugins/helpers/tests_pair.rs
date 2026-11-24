use std::collections::HashSet;
use std::sync::Arc;

use marspot::pidtree::ProcId;
use marspot_peer::control::AgentKind;

use super::{LOOK_EVERY_MS, Move, Moved, NAMED_WITHIN_MS, look_for_names, note_move, pair_in, write_moves};
use crate::plugins::helpers::table::{Event, Started};
use crate::plugins::helpers::tests::{feed, shared_at, started, temp};
use crate::plugins::helpers::work::Job;
use crate::plugins::helpers::{Shared, rebuild, records, timers};
use crate::plugins::peerbus::Conversation;

const OLD: ProcId = ProcId { pid: 50, start_us: 5 };
const NEW: ProcId = ProcId { pid: 60, start_us: 6 };

fn in_conversation(agent: ProcId, id: &str) -> Conversation {
    Conversation { agent, id: id.into() }
}

/// Pane 1 handed from `left` (in `from`) to `to`, going on with `resumed`.
fn moved(from: Option<&str>, left: ProcId, to: ProcId, resumed: Option<&str>) -> Move {
    Move { m: Moved { pane: 1, from: from.map(String::from), left, resumed: resumed.map(String::from) }, to }
}

/// Move `m`, written down at 1 ms after the epoch.
fn handed(s: &Shared, m: Move) {
    note_move(s, m);
    write_moves(s, 1);
}

fn by(conv: &str) -> Event {
    Event::Started(Started { conv: Some(conv.into()), ..started(1, "claude", AgentKind::Codex) })
}

#[test]
fn helpers_only_a_helper_and_its_parent_are_a_pair() {
    let dir = temp("pair");
    let (s, _rx) = shared_at(&dir);
    for (sid, parent) in [(10, 1), (11, 1), (20, 10), (30, 2)] {
        feed(&s, sid, vec![Event::Started(started(parent, "claude", AgentKind::Codex))]);
    }
    let pair = |a, b| pair_in(&s, a, b, |_| Some(in_conversation(OLD, "any")));
    for (a, b) in [(10, 1), (11, 1), (20, 10), (30, 2)] {
        assert!(pair(a, b) && pair(b, a), "{a} {b}");
    }
    for (a, b) in [(10, 11), (20, 1), (10, 2), (30, 1), (1, 2), (10, 10), (7, 8)] {
        assert!(!pair(a, b) && !pair(b, a), "{a} {b}");
    }
    feed(&s, 10, vec![Event::Stopped { exited: false }]);
    assert!(!pair(10, 1), "a helper that went is none");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_parent_pane_with_another_conversation_is_no_pair() {
    let dir = temp("pairconv");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    let pair = |a, b, now: Option<&str>| {
        pair_in(&s, a, b, |pane| {
            assert_eq!(pane, 1, "the parent's");
            now.map(|id| in_conversation(OLD, id))
        })
    };
    // moved to another account, the same conversation goes on
    assert!(pair(10, 1, Some("one")) && pair(1, 10, Some("one")));
    assert!(!pair(10, 1, Some("two")) && !pair(1, 10, Some("two")) && !pair(10, 1, None));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_follow_their_parent_to_the_agent_it_is_handed_to() {
    let dir = temp("handed");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    feed(&s, 11, vec![by("earlier")]);
    let pair = |sid, proc, conv: &str| pair_in(&s, sid, 1, |_| Some(in_conversation(proc, conv)));
    let carried = || {
        let ev = Event::Carried(Box::new(s.table().map[&10].clone()));
        assert_eq!(records::parse(&records::line(1, 1, 10, &ev)), Some((1, 10, ev)));
    };
    assert!(!pair(10, NEW, "two"), "nothing was handed over yet");

    // to a new conversation, which has no name until the agent says so
    handed(&s, moved(Some("one"), OLD, NEW, None));
    assert!(!pair_in(&s, 10, 1, |_| None), "no name yet");
    assert!(!pair(10, ProcId { pid: 60, start_us: 7 }, "x"), "not the agent it was handed to");
    carried();
    assert!(pair(10, NEW, "two") && pair(10, NEW, "two"));
    assert!(!pair(10, NEW, "three"), "and then it is that conversation's");
    assert!(!pair(11, NEW, "two"), "started by an earlier conversation");
    carried();

    // and back, to the one the pane had there
    handed(&s, moved(Some("two"), NEW, OLD, Some("one")));
    assert!(pair(10, OLD, "one") && !pair(10, NEW, "two"));
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table, *s.table());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_handed_on_before_the_agent_named_its_conversation_still_follow() {
    let dir = temp("handedon");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    handed(&s, moved(Some("one"), OLD, NEW, None));
    let third = ProcId { pid: 70, start_us: 7 };
    handed(&s, moved(None, NEW, third, None));
    assert!(pair_in(&s, 10, 1, |_| Some(in_conversation(third, "three"))));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_two_moves_before_the_worker_comes_to_them_are_one() {
    let dir = temp("movetwice");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    let third = ProcId { pid: 80, start_us: 8 };
    note_move(&s, moved(Some("one"), OLD, NEW, None));
    note_move(&s, moved(None, NEW, third, None));
    // another agent's move of the pane takes none of them along
    note_move(&s, moved(None, ProcId { pid: 90, start_us: 9 }, OLD, None));
    write_moves(&s, 1);
    assert_eq!(s.table().map[&10].heir, Some(third));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_move_is_written_down_by_the_worker_with_its_queue_full() {
    let dir = temp("movefull");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    rebuild::begin(&s, HashSet::new).unwrap();
    feed(&s, 10, vec![by("one")]);
    while s.queue(Job::Wake) {}
    note_move(&s, moved(Some("one"), OLD, NEW, None));
    assert!(within(|| s.table().map[&10].heir == Some(NEW)));
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_follow_the_first_conversation_their_parent_was_handed_to_not_one_after_a_clear() {
    let dir = temp("handedclear");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    handed(&s, moved(Some("one"), OLD, NEW, None));
    assert_eq!(timers::sleep_for(&s).map(|d| d.as_millis() as u64 <= LOOK_EVERY_MS), Some(true), "looked for its name");
    look_for_names(&s, 2, |_| None);
    look_for_names(&s, 3, |_| Some(in_conversation(OLD, "one")));
    look_for_names(&s, 4, |_| Some(in_conversation(NEW, "two")));
    assert_eq!(timers::sleep_for(&s), None, "found");
    // the user cleared it before anything asked which conversation it was in
    assert!(!pair_in(&s, 10, 1, |_| Some(in_conversation(NEW, "three"))), "a /clear is a new conversation");
    assert!(pair_in(&s, 10, 1, |_| Some(in_conversation(NEW, "two"))));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_name_is_looked_for_a_while_and_again_after_a_start() {
    let dir = temp("handedlate");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    handed(&s, moved(Some("one"), OLD, NEW, None));
    let t = s.table();
    assert_eq!(super::unnamed_at_start(&t, 5), [(1, 5 + NAMED_WITHIN_MS)].into());
    look_for_names(&s, 1 + NAMED_WITHIN_MS, |_| panic!("not looked at past the while"));
    assert_eq!(timers::sleep_for(&s), None);
    assert_eq!(s.table().map[&10].heir, Some(NEW), "a later look at the pair still settles it");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Wait up to 10 s for `done`.
fn within(done: impl Fn() -> bool) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}
