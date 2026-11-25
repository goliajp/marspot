use std::path::Path;
use std::time::Instant;

use marspot::pidtree::ProcTable;
use marspot::shell_proto::StripState;
use marspot_peer::control::AgentKind;

use super::super::pending::Notice;
use super::super::table::{Event, Started};
use super::super::tests::{feed, shared_at, started, temp};
use super::super::{records, rebuild};
use super::*;

/// A moment well before now.
pub(super) const T: u64 = 1_700_000_000_000;

/// Live panes and whose shadow each is, in front of which nothing runs.
pub(super) fn panes(list: &[(u64, u64)]) -> Arc<Observation> {
    let one = |(sid, parent): &(u64, u64)| {
        let mut e = crate::observe::tests::entry(*sid, 1, 0);
        e.shadow_of = *parent;
        (e, true)
    };
    Arc::new(Observation::from_parts(Instant::now(), ProcTable::from_rows(Vec::new()), list.iter().map(one).collect(), |_| None))
}

pub(super) fn number(sid: u64) -> Option<u16> {
    Some(1000 + sid as u16)
}

pub(super) fn begin(s: &Shared, sid: u64, parent: u64, agent: AgentKind) {
    feed(s, sid, vec![Event::started(started(parent, "claude", agent)), Event::TaskDelivered]);
}

/// The records as of `now_ms`, seen with `obs`; the facts found are
/// written down, as the worker does.
pub(super) fn strips_at(s: &Shared, m: &mut Memory, obs: &Observation, now_ms: u64) -> Vec<HelperStrip> {
    ask_numbers(m, &s.table(), obs, &number);
    let (strips, facts) = compose(&mut s.lock(), obs, m, now_ms);
    for (sid, at, ev) in facts {
        s.record_seen(sid, at, ev);
    }
    strips
}

pub(super) fn strips(s: &Shared, m: &mut Memory, obs: &Observation) -> Vec<HelperStrip> {
    strips_at(s, m, obs, records::now_ms())
}

pub(super) fn at(s: &Shared, sid: u64, at_ms: u64, evs: Vec<Event>) {
    for ev in evs {
        s.record_at(sid, at_ms, ev);
    }
}

pub(super) fn begin_at(s: &Shared, sid: u64, parent: u64, at_ms: u64) {
    at(s, sid, at_ms, vec![Event::started(started(parent, "claude", AgentKind::Codex)), Event::TaskDelivered]);
}

fn states(x: &HelperStrip) -> Vec<(u64, StripState, bool)> {
    x.rows.iter().map(|r| (r.sid, r.state, r.gone)).collect()
}

pub(super) fn dir_gone(d: &Path) {
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn lists_helpers_by_their_top_pane_in_start_order() {
    let d = temp("strip-top");
    let (s, _rx) = shared_at(&d);
    let obs = panes(&[(10, 0), (20, 0), (30, 20), (3, 10), (1, 3), (2, 30), (4, 10)]);
    begin(&s, 3, 10, AgentKind::Codex);
    std::thread::sleep(std::time::Duration::from_millis(2));
    begin(&s, 1, 3, AgentKind::Gemini);
    begin(&s, 2, 30, AgentKind::Kimi);
    // its pane is open, its task has not gone in
    feed(&s, 4, vec![Event::started(started(10, "claude", AgentKind::Claude))]);
    let got = strips(&s, &mut Memory::default(), &obs);
    let lists: Vec<(u64, Vec<u64>)> = got.iter().map(|x| (x.root_sid, x.rows.iter().map(|r| r.sid).collect())).collect();
    assert_eq!(lists, [(10, vec![3, 1]), (20, vec![2])]);
    assert_eq!(got[0].rows[0].number, Some(1003));
    dir_gone(&d);
}

#[test]
fn a_start_that_fails_yields_no_row_and_preparing_does() {
    let d = temp("strip-fail");
    let (s, _rx) = shared_at(&d);
    let obs = panes(&[(10, 0), (1, 10), (2, 10), (3, 10)]);
    let mut m = Memory::default();
    begin(&s, 1, 10, AgentKind::Codex);
    feed(&s, 2, vec![Event::started(started(10, "claude", AgentKind::Codex)), Event::Stopped { exited: false }]);
    feed(&s, 3, vec![Event::started(started(10, "claude", AgentKind::Codex)), Event::Preparing]);
    assert_eq!(states(&strips(&s, &mut m, &obs)[0]), [(1, StripState::Working, false), (3, StripState::Preparing, false)]);
    // it was listed: that it failed is, too
    feed(&s, 3, vec![Event::Stopped { exited: false }]);
    assert_eq!(states(&strips(&s, &mut m, &obs)[0]), [(1, StripState::Working, false), (3, StripState::Exited, true)]);
    dir_gone(&d);
}

#[test]
fn screen_judged_helpers_follow_what_was_told() {
    let d = temp("strip-screen");
    let (s, _rx) = shared_at(&d);
    let obs = panes(&[(10, 0), (1, 10)]);
    let mut m = Memory::default();
    let state = |s: &Shared, m: &mut Memory| {
        let r = strips(s, m, &obs)[0].rows[0].clone();
        assert_eq!(r.ended_ms.is_some(), r.state == StripState::Idle, "its time stands still while it is idle");
        r.state
    };
    begin(&s, 1, 10, AgentKind::Codex);
    assert_eq!(state(&s, &mut m), StripState::Working);
    feed(&s, 1, vec![Event::IdleNoticed]);
    assert_eq!(state(&s, &mut m), StripState::Idle);
    feed(&s, 1, vec![Event::InputSeen]);
    assert_eq!(state(&s, &mut m), StripState::Working);
    feed(&s, 1, vec![Event::BlockedNoticed { prompt: String::new() }]);
    assert_eq!(state(&s, &mut m), StripState::Waiting);
    feed(&s, 1, vec![Event::InputSeen]);
    let n = Notice { parent: 10, events: vec![Event::BlockedNoticed { prompt: String::new() }], ..Notice::default() };
    s.lock().undelivered.keep(1, n);
    assert_eq!(state(&s, &mut m), StripState::Waiting, "though nobody could be told");
    dir_gone(&d);
}

#[test]
fn turn_reported_helpers_follow_their_turn() {
    let d = temp("strip-turn");
    let (s, _rx) = shared_at(&d);
    let obs = panes(&[(10, 0), (1, 10)]);
    let mut m = Memory::default();
    begin(&s, 1, 10, AgentKind::Claude);
    let mut at = |turn: Option<Turn>| {
        s.publish_turns(&turn.map(|t| (1, t)).into_iter().collect());
        strips(&s, &mut m, &obs)[0].rows[0].state
    };
    assert_eq!(at(None), StripState::Working, "its first turn is about to start");
    assert_eq!(at(Some(Turn::Idle)), StripState::Working);
    s.lock().turned.insert(1);
    s.at_work(1, true);
    for (t, want) in [
        (Turn::Working, StripState::Working),
        (Turn::Held, StripState::Working),
        (Turn::BlockedTold, StripState::Waiting),
        (Turn::BlockedBefore, StripState::Waiting),
        (Turn::Idle, StripState::Idle),
    ] {
        assert_eq!(at(Some(t)), want, "{t:?}");
    }
    // drawing after its end is not another turn
    s.at_work(1, false);
    assert_eq!(at(Some(Turn::Working)), StripState::Idle);
    s.at_work(1, true);
    assert_eq!(at(Some(Turn::Working)), StripState::Working);
    assert_eq!(at(Some(Turn::Idle)), StripState::Idle);
    let before = s.strip_gen.load(Ordering::Acquire);
    s.publish_turns(&HashMap::from([(1, Turn::Idle)]));
    assert_eq!(s.strip_gen.load(Ordering::Acquire), before, "the same turns are no change");
    dir_gone(&d);
}

#[test]
fn elapsed_leaves_out_the_time_between_two_turns() {
    let d = temp("strip-elapsed");
    let (s, _rx) = shared_at(&d);
    let (obs, mut m) = (panes(&[(10, 0), (1, 10), (2, 10)]), Memory::default());
    begin_at(&s, 1, 10, T);
    begin_at(&s, 2, 10, T);
    strips_at(&s, &mut m, &obs, T + 100);
    at(&s, 1, T + 5_000, vec![Event::IdleNoticed]);
    let r = strips_at(&s, &mut m, &obs, T + 5_000)[0].rows[0].clone();
    assert_eq!((r.worked_ms, r.running_since_ms, r.ended_ms, r.elapsed_ms(T + 9_000)), (5_000, None, Some(T + 5_000), 5_000));
    at(&s, 1, T + 60_000, vec![Event::InputSeen]);
    let r = strips_at(&s, &mut m, &obs, T + 60_010)[0].rows[0].clone();
    assert_eq!((r.state, r.ended_ms, r.elapsed_ms(T + 61_000)), (StripState::Working, None, 6_000));
    // waiting for an answer, it is at work
    at(&s, 1, T + 62_000, vec![Event::BlockedNoticed { prompt: String::new() }]);
    let r = strips_at(&s, &mut m, &obs, T + 63_000)[0].rows[0].clone();
    assert_eq!((r.state, r.ended_ms, r.elapsed_ms(T + 64_000)), (StripState::Waiting, None, 9_000));
    dir_gone(&d);
}

#[test]
fn a_batch_is_over_three_seconds_after_its_last_end() {
    let d = temp("strip-batch");
    let (s, _rx) = shared_at(&d);
    let (obs, mut m) = (panes(&[(10, 0), (1, 10), (2, 10), (3, 10)]), Memory::default());
    for sid in 1..=3 {
        begin_at(&s, sid, 10, T + sid);
    }
    s.used(1, "c", used::Used { tokens: Some(100), had_tool: Some(false), model: None });
    let a = strips_at(&s, &mut m, &obs, T + 10);
    assert_eq!(a[0].batch, T + 1);
    at(&s, 1, T + 100, vec![Event::IdleNoticed]);
    strips_at(&s, &mut m, &obs, T + 100);
    at(&s, 1, T + 50_000, vec![Event::Stopped { exited: false }]);
    at(&s, 2, T + 50_000, vec![Event::ExitedNoticed, Event::Stopped { exited: false }]);
    let left = panes(&[(10, 0), (3, 10)]);
    let a = strips_at(&s, &mut m, &left, T + 90_000);
    assert_eq!(states(&a[0]), [(1, StripState::Idle, true), (2, StripState::Exited, true), (3, StripState::Working, false)]);
    let one = &a[0].rows[0];
    assert_eq!((one.ended_ms, one.tokens, one.had_tool), (Some(T + 100), Some(100), false));
    assert_eq!(s.lock().batches.next_ms(&s.table()), None, "nothing is waited for while one is at work");
    // the last ends its turn; its pane stays
    at(&s, 3, T + 91_000, vec![Event::IdleNoticed]);
    assert_eq!(strips_at(&s, &mut m, &left, T + 91_000)[0].rows.len(), 3);
    assert_eq!(s.lock().batches.next_ms(&s.table()), Some(T + 94_000));
    assert_eq!(strips_at(&s, &mut m, &left, T + 93_999).len(), 1);
    assert!(strips_at(&s, &mut m, &left, T + 94_000).is_empty());
    assert_eq!(s.lock().batches.next_ms(&s.table()), None);
    // back at work, it is a batch of its own
    at(&s, 3, T + 100_000, vec![Event::InputSeen]);
    let b = strips_at(&s, &mut m, &left, T + 100_000);
    assert_eq!((states(&b[0]), b[0].batch), (vec![(3, StripState::Working, false)], T + 100_000));
    assert_eq!(b[0].rows[0].elapsed_ms(T + 100_500), 91_000 - 3 + 500);
    assert_eq!((m.numbers.len(), s.lock().used.len()), (1, 0));
    dir_gone(&d);
}

#[test]
fn a_row_closed_at_work_stays_thirty_seconds_on_its_own() {
    let d = temp("strip-stopped");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    for sid in 1..=3 {
        begin_at(&s, sid, 10, T);
    }
    at(&s, 1, T + 1_000, vec![Event::Stopped { exited: false }]);
    at(&s, 2, T + 20_000, vec![Event::Stopped { exited: false }]);
    let obs = panes(&[(10, 0), (3, 10)]);
    assert_eq!(strips_at(&s, &mut m, &obs, T + 30_999)[0].rows.len(), 3);
    assert_eq!(s.lock().batches.next_ms(&s.table()), Some(T + 31_000), "its own end is waited for");
    let a = strips_at(&s, &mut m, &obs, T + 31_000);
    assert_eq!(states(&a[0]), [(2, StripState::Stopped, true), (3, StripState::Working, false)]);
    at(&s, 3, T + 32_000, vec![Event::ExitedNoticed, Event::Stopped { exited: false }]);
    assert_eq!(strips_at(&s, &mut m, &obs, T + 49_999)[0].rows.len(), 2, "it keeps the batch");
    assert!(strips_at(&s, &mut m, &obs, T + 50_000).is_empty());
    dir_gone(&d);
}

#[test]
fn a_batch_survives_a_start_of_the_shell_and_one_long_over_does_not() {
    let d = temp("strip-restart");
    let (s, _rx) = shared_at(&d);
    let (obs, mut m) = (panes(&[(10, 0), (20, 0), (3, 10), (4, 3), (7, 20)]), Memory::default());
    let now = records::now_ms();
    let t = now - 100_000;
    // an earlier batch
    begin_at(&s, 1, 10, t);
    at(&s, 1, t + 1_000, vec![Event::ExitedNoticed, Event::Stopped { exited: false }]);
    for (sid, parent) in [(2, 10), (3, 10), (4, 3), (5, 3)] {
        begin_at(&s, sid, parent, t + 10_000 + sid);
    }
    begin_at(&s, 7, 20, t);
    strips_at(&s, &mut m, &obs, t + 10_100);
    at(&s, 2, t + 11_000, vec![Event::IdleNoticed]);
    at(&s, 7, t + 11_000, vec![Event::IdleNoticed]);
    strips_at(&s, &mut m, &obs, t + 11_000);
    at(&s, 2, t + 12_000, vec![Event::Stopped { exited: false }]);
    at(&s, 5, now - 2_000, vec![Event::Stopped { exited: false }]);
    let before = strips_at(&s, &mut m, &obs, now);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].batch, t + 10_002);
    let (again, _rx2) = shared_at(&d);
    rebuild::rebuild(&again, || HashSet::from([3, 4, 7]));
    let after = strips_at(&again, &mut Memory::default(), &obs, now);
    assert_eq!(states(&after[0]), [(2, StripState::Idle, true), (3, StripState::Working, false), (4, StripState::Working, false), (5, StripState::Stopped, true)]);
    assert_eq!(after, before);
    assert_eq!(again.table(), s.table());
    dir_gone(&d);
}

#[test]
fn the_top_pane_is_the_one_at_the_start_until_a_place_changes_it() {
    let d = temp("strip-root");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    let with_root = |parent, root| Started { root: Some(root), ..started(parent, "claude", AgentKind::Codex) };
    feed(&s, 3, vec![Event::started(with_root(10, 10)), Event::TaskDelivered]);
    feed(&s, 4, vec![Event::started(with_root(3, 10)), Event::TaskDelivered]);
    // the pane between is gone, its row too
    feed(&s, 3, vec![Event::IdleNoticed]);
    s.lock().table.map.remove(&3);
    let got = strips(&s, &mut m, &panes(&[(10, 0), (4, 3)]));
    assert_eq!((got[0].root_sid, got[0].rows.iter().map(|r| r.sid).collect::<Vec<_>>()), (10, vec![4]));
    // placed under another: written down
    let got = strips(&s, &mut m, &panes(&[(10, 0), (20, 0), (4, 20)]));
    assert_eq!(got.iter().map(|x| x.root_sid).collect::<Vec<_>>(), [20]);
    let replayed = records::replay(&s.log, &|_| None).unwrap();
    assert_eq!(replayed.table.map[&4].root, Some(20));
    assert_eq!(s.table().map[&4].root, Some(20));
    dir_gone(&d);
}

#[test]
fn a_helper_moved_to_a_window_is_listed_nowhere() {
    let d = temp("strip-moved");
    let (s, _rx) = shared_at(&d);
    let mut m = Memory::default();
    begin(&s, 1, 10, AgentKind::Codex);
    begin(&s, 2, 10, AgentKind::Codex);
    begin(&s, 3, 10, AgentKind::Codex);
    feed(&s, 3, vec![Event::Stopped { exited: false }]);
    let got = strips(&s, &mut m, &panes(&[(10, 0), (1, 10), (2, 0)]));
    assert_eq!(got.len(), 1);
    assert_eq!(states(&got[0]), [(1, StripState::Working, false), (3, StripState::Stopped, true)]);
    // closed there: still not the parent's, though its session is gone
    feed(&s, 2, vec![Event::Stopped { exited: false }]);
    assert_eq!(states(&strips(&s, &mut m, &panes(&[(10, 0), (1, 10)]))[0]), [(1, StripState::Working, false), (3, StripState::Stopped, true)]);
    // the last listed one moved
    assert_eq!(states(&strips(&s, &mut m, &panes(&[(10, 0), (1, 0)]))[0]), [(3, StripState::Stopped, true)]);
    assert_eq!(strips(&s, &mut m, &panes(&[(10, 0), (1, 10)]))[0].rows.len(), 2, "and back");
    dir_gone(&d);
}

#[test]
fn rows_go_in_start_order_and_what_stopped_longest_ago_goes_first() {
    let d = temp("strip-more");
    let (s, _rx) = shared_at(&d);
    let t = records::now_ms() - 1_000;
    for sid in 1..=30 {
        begin_at(&s, sid, 10, t + sid);
    }
    for sid in 1..=8 {
        at(&s, sid, t + 100 + 9 - sid, vec![Event::Stopped { exited: false }]);
    }
    let live: Vec<(u64, u64)> = std::iter::once((10, 0)).chain((9..=30).filter(|s| *s != 10).map(|s| (s, 10))).collect();
    let got = strips(&s, &mut Memory::default(), &panes(&live));
    let sids: Vec<u64> = got[0].rows.iter().map(|r| r.sid).collect();
    assert_eq!(sids, [1, 2, 3].into_iter().chain((9..=30).filter(|s| *s != 10)).collect::<Vec<_>>());
    dir_gone(&d);
}

#[test]
fn stopped_rows_of_one_pane_are_bounded() {
    let d = temp("strip-cap");
    let (s, _rx) = shared_at(&d);
    begin(&s, 1000, 10, AgentKind::Codex);
    for sid in 1..=40 {
        begin(&s, sid, 10, AgentKind::Codex);
        feed(&s, sid, vec![Event::Stopped { exited: false }]);
    }
    assert_eq!(s.lock().batches.ended().len(), HELPER_STRIP_ROWS);
    assert_eq!(s.lock().batches.ended()[0].helper.sid, 17, "the oldest went");
    dir_gone(&d);
}

#[test]
fn declares_on_change_and_empty_when_the_batch_is_over() {
    let d = temp("strip-declare");
    let (s, _rx) = shared_at(&d);
    let (mut decls, mut m) = (PaneDecls::new(), Memory::default());
    // 11 is a plain shadow pane of 10: nothing is said of it
    let mut obs = panes(&[(10, 0), (11, 10), (20, 0)]);
    let reads = std::cell::Cell::new(0);
    let sent = |decls: &mut PaneDecls<HelperStrip>| {
        let mut out = Vec::new();
        decls.flush(|root, x| {
            assert_eq!(root, x.root_sid);
            out.push((root, x.rows.len()));
            true
        });
        out
    };
    let pass = |decls: &mut PaneDecls<HelperStrip>, m: &mut Memory, obs: &Arc<Observation>| {
        let look = || {
            reads.set(reads.get() + 1);
            Arc::clone(obs)
        };
        declare_in(decls, &s, m, &look, &number);
    };
    pass(&mut decls, &mut m, &obs);
    assert_eq!(sent(&mut decls), []);
    pass(&mut decls, &mut m, &obs);
    assert_eq!((sent(&mut decls), reads.get()), (vec![], 0), "with no helpers nothing is looked at");
    begin(&s, 1, 10, AgentKind::Codex);
    obs = panes(&[(10, 0), (11, 10), (20, 0), (1, 10)]);
    pass(&mut decls, &mut m, &obs);
    assert_eq!(sent(&mut decls), [(10, 1)]);
    // a fresh look at the same panes, and a fact the strip does not show
    obs = panes(&[(10, 0), (11, 10), (20, 0), (1, 10)]);
    feed(&s, 1, vec![Event::ResultChanged]);
    pass(&mut decls, &mut m, &obs);
    assert_eq!(sent(&mut decls), []);
    decls.core_replaced();
    assert_eq!(sent(&mut decls), [(10, 1)], "a new core is told again");
    feed(&s, 1, vec![Event::ExitedNoticed, Event::Stopped { exited: false }]);
    pass(&mut decls, &mut m, &obs);
    assert_eq!(sent(&mut decls), [(10, 1)], "a moment longer");
    let g = s.strip_gen.load(Ordering::Acquire);
    s.expire_strips(records::now_ms());
    assert_eq!(s.strip_gen.load(Ordering::Acquire), g);
    s.expire_strips(records::now_ms() + 3_000);
    pass(&mut decls, &mut m, &obs);
    assert_eq!(sent(&mut decls), [(10, 0)]);
    decls.core_replaced();
    assert_eq!(sent(&mut decls), [(10, 0)], "known to have none is told, too");
    let n = reads.get();
    pass(&mut decls, &mut m, &obs);
    assert_eq!(reads.get(), n);
    dir_gone(&d);
}

#[test]
fn says_which_agent_is_in_front_of_the_parent() {
    let shell = crate::observe::tests::row(50, 1, "zsh");
    for (comm, want) in [("claude", StripAgent::Claude), ("codex", StripAgent::Codex), ("vim", StripAgent::Other)] {
        let mut front = crate::observe::tests::row(60, 50, comm);
        (front.pgid, front.tty_fg_pgid) = (60, 60);
        let mut sh = shell.clone();
        sh.tty_fg_pgid = 60;
        let e = crate::observe::tests::entry(10, 1, 50);
        let obs = Observation::from_parts(Instant::now(), ProcTable::from_rows(vec![sh, front]), vec![(e, true)], |r| Some(Arc::from([r.comm.clone()])));
        assert_eq!(agent_at(&obs, 10), want, "{comm}");
    }
    assert_eq!(agent_at(&panes(&[(10, 0)]), 10), StripAgent::Other);
}

#[test]
fn context_read_is_kept_for_helpers_outside_containers() {
    let d = temp("strip-used");
    let (s, _rx) = shared_at(&d);
    let read = |tokens, had_tool| used::Used { tokens, had_tool, model: None };
    begin(&s, 1, 10, AgentKind::Claude);
    let boxed = Started { isolation: marspot_peer::control::Isolation::Container, ..started(10, "claude", AgentKind::Claude) };
    feed(&s, 2, vec![Event::started(boxed), Event::TaskDelivered]);
    for sid in [1, 2, 3] {
        s.used(sid, "c", read(Some(12_010), Some(false)));
    }
    let g = s.strip_gen.load(Ordering::Acquire);
    s.used(1, "c", read(Some(12_010), None));
    assert_eq!(s.strip_gen.load(Ordering::Acquire), g, "nothing new");
    let obs = panes(&[(10, 0), (1, 10), (2, 10), (3, 10)]);
    let rows = |s: &Shared| strips(s, &mut Memory::default(), &obs)[0].rows.iter().map(|r| (r.sid, r.tokens, r.had_tool)).collect::<Vec<_>>();
    assert_eq!(rows(&s), [(1, Some(12_010), false), (2, None, true)]);
    s.used(1, "c", read(None, Some(true)));
    s.used(1, "c", read(Some(13_041), Some(false)));
    assert_eq!(rows(&s)[0], (1, Some(13_041), true), "a tool called once was called");
    // another conversation in the pane
    s.used(1, "d", used::Used::default());
    assert_eq!(rows(&s)[0], (1, None, true));
    s.used(1, "d", read(Some(900), Some(false)));
    assert_eq!(rows(&s)[0], (1, Some(900), false));
    dir_gone(&d);
}
