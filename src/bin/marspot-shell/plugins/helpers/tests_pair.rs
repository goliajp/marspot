use marspot::pidtree::ProcId;
use marspot_peer::control::AgentKind;

use super::{Look, Moved, note_move, pair_in, write_moves};
use crate::plugins::helpers::records;
use crate::plugins::helpers::table::{Event, Started};
use crate::plugins::helpers::Shared;
use crate::plugins::helpers::tests::{feed, shared_at, started, temp};
use crate::plugins::helpers::work::Job;
use crate::plugins::peerbus::Conversation;

const OLD: ProcId = ProcId { pid: 50, start_us: 5 };
const NEW: ProcId = ProcId { pid: 60, start_us: 6 };

fn in_conversation(agent: ProcId, id: &str) -> Conversation {
    Conversation { agent, id: id.into() }
}

/// Move `m` at 1 ms after the epoch, written down with `front` there.
fn handed(s: &Shared, m: Moved, front: Option<ProcId>) {
    note_move(s, m, 1_000);
    write_moves(s, |_| Look { front, conv: None });
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
    let m = Moved { pane: 1, from: Some("one".into()), left: OLD, resumed: None };
    handed(&s, m, Some(NEW));
    assert!(!pair_in(&s, 10, 1, |_| None), "no name yet");
    assert!(!pair(10, ProcId { pid: 60, start_us: 7 }, "x"), "not the agent it was handed to");
    carried();
    assert!(pair(10, NEW, "two") && pair(10, NEW, "two"));
    assert!(!pair(10, NEW, "three"), "and then it is that conversation's");
    assert!(!pair(11, NEW, "two"), "started by an earlier conversation");
    carried();

    // and back, to the one the pane had there
    let m = Moved { pane: 1, from: Some("two".into()), left: NEW, resumed: Some("one".into()) };
    handed(&s, m, None);
    assert!(pair(10, OLD, "one") && !pair(10, NEW, "two"));
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table, *s.table());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_handed_on_before_the_agent_named_its_conversation_still_follow() {
    let dir = temp("handedon");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    let m = |from: Option<&str>, left| Moved { pane: 1, from: from.map(String::from), left, resumed: None };
    handed(&s, m(Some("one"), OLD), Some(NEW));
    let third = ProcId { pid: 70, start_us: 7 };
    handed(&s, m(None, NEW), Some(third));
    assert!(pair_in(&s, 10, 1, |_| Some(in_conversation(third, "three"))));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_move_is_kept_with_the_queue_full_and_taken_by_the_agent_there_then() {
    let dir = temp("movefull");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    while s.queue(Job::Wake) {}
    let m = |from: Option<&str>, left| Moved { pane: 1, from: from.map(String::from), left, resumed: None };
    note_move(&s, m(Some("one"), OLD), 1_000);
    write_moves(&s, |_| Look { front: Some(NEW), conv: None });
    assert_eq!(s.table().map[&10].heir, Some(NEW), "kept although no job could be queued");

    // a program started after the move is not the agent it was handed to
    note_move(&s, m(None, NEW), 2_000);
    let later = ProcId { pid: 70, start_us: 2_001 };
    write_moves(&s, |_| Look { front: Some(later), conv: None });
    assert_eq!(s.table().map[&10].heir, Some(NEW));

    // two moves before the worker comes to them are one
    let third = ProcId { pid: 80, start_us: 2_500 };
    note_move(&s, m(None, NEW), 3_000);
    note_move(&s, m(None, third), 4_000);
    let fourth = ProcId { pid: 90, start_us: 3_500 };
    write_moves(&s, |_| Look { front: Some(fourth), conv: None });
    assert_eq!(s.table().map[&10].heir, Some(fourth));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_follow_the_conversation_their_parent_was_handed_to_not_one_after_a_clear() {
    let dir = temp("handedclear");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 10, vec![by("one")]);
    note_move(&s, Moved { pane: 1, from: Some("one".into()), left: OLD, resumed: None }, 1_000);
    write_moves(&s, |_| Look { front: Some(NEW), conv: Some(in_conversation(NEW, "two")) });
    // the user cleared it before anything asked which conversation it was in
    assert!(!pair_in(&s, 10, 1, |_| Some(in_conversation(NEW, "three"))), "a /clear is a new conversation");
    assert!(pair_in(&s, 10, 1, |_| Some(in_conversation(NEW, "two"))));
    std::fs::remove_dir_all(&dir).unwrap();
}
