use std::cell::Cell;
use std::sync::mpsc::Receiver;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::shell_proto::StripState;
use marspot_peer::control::AgentKind;

use super::super::table::{Event, Started};
use super::super::tests::{host_with, shared_at, started, temp};
use super::super::turn::{poked_at_start, turn, turns_at_start};
use super::super::{rebuild, records};
use super::helper_strip_tests::{T, at, begin, begin_at, dir_gone, number, panes, strips_at};
use super::*;

fn over(sid: u64, at: u64) {
    turns::publish(vec![(sid, Look::Over { began: None, at, quiet_after: None })]);
}

/// A shell that starts over on the log in `d`, its plugin started and
/// no pane's state known yet.
fn again(d: &std::path::Path, live: &[u64]) -> (Arc<Shared>, Receiver<Job>) {
    let (s, rx) = shared_at(d);
    rebuild::rebuild(&s, || live.iter().copied().collect());
    s.publish_turns(&turns_at_start(&s.table()));
    (s, rx)
}

#[test]
fn a_later_turn_goes_on_over_a_start_of_the_shell() {
    let d = temp("strip-later");
    let (s, _rx) = shared_at(&d);
    let (obs, mut m) = (panes(&[(10, 0), (1, 10), (2, 10)]), Memory::default());
    let now = records::now_ms();
    let t = now - 600_000;
    for (sid, agent) in [(1, AgentKind::Claude), (2, AgentKind::Kimi)] {
        at(&s, sid, t, vec![Event::started(started(10, "claude", agent)), Event::TaskDelivered]);
        s.at_work(sid, true);
    }
    s.publish_turns(&HashMap::from([(1, Turn::Working), (2, Turn::Working)]));
    strips_at(&s, &mut m, &obs, t + 10);
    for sid in [1, 2] {
        at(&s, sid, t + 5_000, vec![Event::IdleNoticed]);
        s.lock().turned.insert(sid);
    }
    s.publish_turns(&HashMap::from([(1, Turn::Idle), (2, Turn::Idle)]));
    assert!(strips_at(&s, &mut m, &obs, t + 5_000)[0].rows.iter().all(|r| r.running_since_ms.is_none()));
    // a message opens another turn: no input is seen here
    s.publish_turns(&HashMap::from([(1, Turn::Working), (2, Turn::Working)]));
    let before = strips_at(&s, &mut m, &obs, t + 60_000);
    assert!(s.table().map.values().all(|h| h.noticed && h.clock.since_ms == Some(t + 60_000)));
    let (s, _rx) = again(&d, &[1, 2]);
    assert!(poked_at_start(&s.table(), now).is_empty());
    let mut m = Memory::default();
    let after = strips_at(&s, &mut m, &obs, now);
    assert_eq!(after, before);
    assert_eq!(after[0].rows[0].worked_ms, 5_000);
    assert_eq!(strips_at(&s, &mut m, &obs, now + 10_000), before);
    let log = std::fs::read_to_string(&s.log).unwrap();
    assert_eq!(log.lines().filter(|l| l.ends_with("turn_ended")).count(), 2, "one each, of the first turn");
    dir_gone(&d);
}

#[test]
fn a_turn_that_ended_while_no_shell_ran_ends_at_its_record() {
    let d = temp("strip-unseen");
    let (s, _rx) = shared_at(&d);
    let (obs, mut m) = (panes(&[(10, 0), (1, 10), (2, 10)]), Memory::default());
    let now = records::now_ms();
    let t = now - 600_000;
    begin_at(&s, 2, 10, t);
    at(&s, 1, t, vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered, Event::IdleNoticed, Event::TurnEnded]);
    at(&s, 1, t + 1_000, vec![Event::TurnBegan]);
    let (s, _rx) = again(&d, &[1, 2]);
    assert_eq!(strips_at(&s, &mut m, &obs, now)[0].rows[0].state, StripState::Working);
    over(1, now - 100_000);
    let (mut in_turn, table) = (turns_at_start(&s.table()), s.table());
    turn(&s, &host_with(&[(1, PaneStatus::AwaitingUser)]), &table, &table.map[&1], now, &mut in_turn);
    s.publish_turns(&in_turn);
    let r = strips_at(&s, &mut m, &obs, now + 1_000)[0].rows[0].clone();
    assert_eq!((r.state, r.ended_ms, r.worked_ms), (StripState::Idle, Some(now - 100_000), 600_000 - 1_000 - 100_000));
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table.map[&1].clock, s.lock().table.map[&1].clock);
    dir_gone(&d);
}

#[test]
fn codes_are_asked_once_for_open_panes_and_not_under_the_lock() {
    let d = temp("strip-codes");
    let (s, _rx) = shared_at(&d);
    let (mut decls, mut m) = (PaneDecls::new(), Memory::default());
    for sid in [1, 2, 3] {
        begin(&s, sid, 10, AgentKind::Codex);
    }
    // the session of 2 is gone; nothing can say the code of 3
    let obs = panes(&[(10, 0), (1, 10), (3, 10)]);
    let asked = std::cell::RefCell::new(Vec::new());
    let ask = |sid| {
        assert!(s.state.try_lock().is_ok(), "the file may be locked for a moment");
        asked.borrow_mut().push(sid);
        number(sid).filter(|_| sid != 3)
    };
    for _ in 0..3 {
        s.strip_changed();
        declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&obs), &ask);
    }
    assert_eq!(*asked.borrow(), [1, 3]);
    at(&s, 1, records::now_ms(), vec![Event::Stopped { exited: false }]);
    declare_in(&mut decls, &s, &mut m, &|| panes(&[(10, 0), (3, 10)]), &ask);
    let codes: Vec<_> = decls.declared(10).unwrap().rows.iter().map(|r| (r.sid, r.number)).collect();
    assert_eq!(codes, [(1, Some(1001)), (2, None), (3, None)]);
    assert_eq!(asked.borrow().len(), 2);
    dir_gone(&d);
}

#[test]
fn a_start_waits_for_a_walk_only_for_a_helper_whose_top_is_not_said() {
    let d = temp("strip-walk");
    let (s, _rx) = shared_at(&d);
    let looks = Cell::new(0);
    let look = || {
        looks.set(looks.get() + 1);
        panes(&[(20, 0), (10, 20), (1, 10), (2, 10)])
    };
    assert!(rebuild::reload(&s, &look));
    let with_root = Started { root: Some(10), ..started(10, "claude", AgentKind::Codex) };
    at(&s, 1, records::now_ms(), vec![Event::started(with_root), Event::TaskDelivered]);
    assert!(rebuild::reload(&s, &look));
    assert_eq!(looks.get(), 0);
    begin(&s, 2, 10, AgentKind::Codex);
    begin(&s, 3, 10, AgentKind::Codex);
    assert!(rebuild::reload(&s, &look));
    assert_eq!(looks.get(), 1);
    assert!(s.lock().batches.open()[&20].open.contains(&2));
    dir_gone(&d);
}

#[test]
fn no_observation_is_kept_once_nothing_is_listed() {
    let d = temp("strip-kept");
    let (s, _rx) = shared_at(&d);
    let (mut decls, mut m) = (PaneDecls::new(), Memory::default());
    begin(&s, 1, 10, AgentKind::Codex);
    let obs = panes(&[(10, 0), (1, 10)]);
    declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&obs), &number);
    at(&s, 1, records::now_ms(), vec![Event::ExitedNoticed, Event::Stopped { exited: false }]);
    s.expire_strips(records::now_ms() + 3_000);
    let last = panes(&[(10, 0)]);
    declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&last), &number);
    assert!(decls.declared(10).is_some_and(HelperStrip::is_empty));
    assert_eq!((Arc::strong_count(&obs), Arc::strong_count(&last)), (1, 1));
    dir_gone(&d);
}

#[test]
fn one_gone_while_no_shell_ran_exited_after_every_start() {
    let d = temp("strip-exited");
    let (s, _rx) = shared_at(&d);
    begin(&s, 1, 10, AgentKind::Codex);
    for _ in 0..2 {
        let (s, _rx) = again(&d, &[]);
        assert_eq!(s.lock().batches.ended().iter().map(|e| (e.helper.sid, e.was_stopped())).collect::<Vec<_>>(), [(1, false)]);
        s.park();
    }
    dir_gone(&d);
}

#[test]
fn what_the_loop_saw_replays_when_it_happened() {
    let d = temp("strip-order");
    let (s, _rx) = shared_at(&d);
    begin_at(&s, 1, 10, T);
    begin_at(&s, 2, 10, T);
    s.lock().table.map.get_mut(&1).unwrap().clock.end(T + 5_000);
    at(&s, 1, T + 6_000, vec![Event::InputSeen]);
    // written a moment later
    s.record_seen(1, T + 5_000, Event::TurnEnded);
    let live = s.table().map[&1].clock;
    assert_eq!((live.worked_ms, live.since_ms), (5_000, Some(T + 6_000)));
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table.map[&1].clock, live);
    dir_gone(&d);
}

#[test]
fn one_placed_elsewhere_replays_in_one_batch() {
    let d = temp("strip-placed");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    let root = |root| Started { root: Some(root), ..started(10, "claude", AgentKind::Codex) };
    for sid in [1, 2] {
        at(&s, sid, T + sid, vec![Event::started(root(10)), Event::TaskDelivered]);
    }
    strips_at(&s, &mut m, &panes(&[(10, 0), (20, 0), (1, 10), (2, 10)]), T + 10);
    strips_at(&s, &mut m, &panes(&[(10, 0), (20, 0), (1, 10), (2, 20)]), T + 20);
    let replayed = records::replay(&s.log, &|_| None).unwrap().batches;
    assert_eq!(replayed.open(), s.lock().batches.open());
    assert_eq!(replayed.open()[&10].open.iter().copied().collect::<Vec<_>>(), [1]);
    // a fact written late does not undo a later move
    strips_at(&s, &mut m, &panes(&[(10, 0), (20, 0), (1, 10), (2, 10)]), T + 30);
    s.record_seen(2, T + 20, Event::Rooted(20));
    assert_eq!(s.table().map[&2].root, Some(10));
    dir_gone(&d);
}

#[test]
fn one_drawing_after_its_recorded_end_is_no_change_each_tick() {
    let d = temp("strip-flap");
    let (s, _rx) = shared_at(&d);
    begin(&s, 1, 10, AgentKind::Claude);
    let now = records::now_ms();
    over(1, now + 1_000);
    let (mut in_turn, t) = (HashMap::new(), s.table());
    let mut tick = || {
        turn(&s, &host_with(&[(1, PaneStatus::Busy(BusyKind::Working))]), &t, &t.map[&1], now, &mut in_turn);
        s.publish_turns(&in_turn);
        (s.strip_gen.load(Ordering::Acquire), s.lock().at_work.contains(&1))
    };
    let first = tick();
    for _ in 0..3 {
        assert_eq!(tick(), first);
    }
    assert!(!first.1);
    dir_gone(&d);
}
