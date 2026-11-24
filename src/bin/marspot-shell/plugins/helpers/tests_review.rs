use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::notify::Stop;
use super::pending::{Notice, TRIES, Undelivered};
use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started, temp};
use super::work::Job;
use super::{Shared, launch, records, stop_all, tick, timers};
use crate::observe::{Observation, Session};
use crate::plugins::TickDue;

/// An observation of panes `(sid, front)`: each live, its shell in
/// front when `front` is 0.
pub(super) fn panes(list: &[(u64, i32)]) -> Observation {
    let sessions = list
        .iter()
        .map(|(sid, front)| Session {
            entry: crate::observe::tests::entry(*sid, 1, 1),
            live: true,
            shell: Some(marspot::pidtree::ProcId { pid: 1, start_us: 1 }),
            descendants: Vec::new(),
            front_pgid: *front,
        })
        .collect();
    Observation { at: Instant::now(), table: marspot::pidtree::ProcTable::from_rows(Vec::new()), sessions }
}

#[test]
fn helpers_tick_due_leaves_a_parked_parent_alone() {
    let dir = temp("parked");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::Started(started(10, "claude", AgentKind::Codex)), Event::TaskDelivered]);
    let now = records::now_ms();
    let shell = panes(&[(10, 0)]);
    let none = HashSet::new();
    let kept = [
        PaneStatus::Busy(BusyKind::StoppedJobs),
        PaneStatus::Busy(BusyKind::BackgroundJobs),
        PaneStatus::Dormant,
        PaneStatus::Unknown,
        PaneStatus::Contradiction { generic: "idle", activity: "awaiting-user" },
    ];
    for state in kept {
        let mut parked = tick::Parked::default();
        // the state may still lag the terminal: watched each second for a while
        tick::reclaim(&s, &host_with(&[(10, state.clone())]), &s.table(), now, &shell, &mut parked);
        assert!(!parked.contains(10) && s.lock().no_agent.is_empty(), "{state:?}");
        assert_eq!(tick::due(&s.table(), &none, &parked, &shell, false, now), TickDue::Cadence);
        tick::reclaim(&s, &host_with(&[(10, state.clone())]), &s.table(), now + tick::SETTLE_MS - 1, &shell, &mut parked);
        assert!(!parked.contains(10), "{state:?}");
        // then it is kept: one look more, and no tick until an event
        tick::reclaim(&s, &host_with(&[(10, state.clone())]), &s.table(), now + tick::SETTLE_MS, &shell, &mut parked);
        assert!(parked.contains(10), "{state:?}");
        assert!(matches!(tick::due(&s.table(), &none, &parked, &shell, false, now), TickDue::At(_)), "one look to come for {state:?}");
        let last = now + tick::SETTLE_MS + tick::RECHECK_MS;
        tick::reclaim(&s, &host_with(&[(10, state.clone())]), &s.table(), last, &shell, &mut parked);
        assert_eq!(tick::due(&s.table(), &none, &parked, &shell, false, last), TickDue::Event, "then no tick for {state:?}");
    }
    // a lagging state that turns empty within the wait is noted
    let mut parked = tick::Parked::default();
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::AwaitingUser)]), &s.table(), now, &shell, &mut parked);
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::Empty)]), &s.table(), now + 2_000, &shell, &mut parked);
    assert!(!parked.contains(10) && s.lock().no_agent.contains_key(&10));
    s.lock().no_agent.clear();
    // an agent took the terminal back: one tick, which forgets it, and
    // its shell back in front starts a new wait
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::Dormant)]), &s.table(), now + 3_000, &shell, &mut parked);
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::Dormant)]), &s.table(), now + 2 * tick::SETTLE_MS, &shell, &mut parked);
    assert!(parked.contains(10));
    let agent = panes(&[(10, 77)]);
    assert_eq!(tick::due(&s.table(), &none, &parked, &agent, false, now), TickDue::Cadence);
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::Dormant)]), &s.table(), now, &agent, &mut parked);
    assert_eq!(tick::due(&s.table(), &none, &parked, &agent, false, now), TickDue::Event);
    assert_eq!(tick::due(&s.table(), &none, &parked, &agent, true, now), TickDue::Cadence, "a notice waits on a new program");
    tick::reclaim(&s, &host_with(&[(10, PaneStatus::Dormant)]), &s.table(), now + 1, &shell, &mut parked);
    assert!(!parked.contains(10));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_wake_takes_one_slot_and_a_dropped_close_frees_its_helper() {
    let dir = temp("wake");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    s.arm(tx);
    s.wake();
    s.wake();
    assert!(matches!(rx.try_recv(), Ok(Job::Wake)));
    assert!(rx.try_recv().is_err(), "one wake however many asked");
    s.wake_queued.store(false, std::sync::atomic::Ordering::Release);
    s.wake();
    // the queue is full now: the close does not go and the helper is free
    s.queue_closing(7, Job::Gone { sid: 7 });
    assert!(!s.lock().closing.contains(&7));
    // a helper being closed has no deadline the worker waits on
    let h = Helper { delivered_ms: Some(1_000), ..Helper::new(7, started(10, "shell", AgentKind::Codex), 500) };
    let mut t = super::table::Table::default();
    t.map.insert(7, h);
    let closing: HashSet<u64> = [7].into();
    assert_eq!(timers::next_ms(&t, &closing, &HashMap::new(), &HashMap::new(), 5_000), None);
    let retry: HashMap<u64, (u64, u32)> = [(7, (9_000, 1))].into();
    assert_eq!(timers::next_ms(&t, &closing, &retry, &HashMap::new(), 5_000), Some(9_000));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_undelivered_notice_waits_for_a_new_program_and_gives_up() {
    let mut u = Undelivered::default();
    let n = |tries| Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed], tries, front: Some(0), ..Notice::default() };
    assert!(u.keep(1, n(1)));
    let same = panes(&[(10, 0)]);
    assert!(!u.waiting_on(&same) && u.due(&same).is_empty(), "no new program: not tried");
    let agent = panes(&[(10, 55)]);
    assert!(u.waiting_on(&agent));
    assert_eq!(u.due(&agent), vec![(1, Some(0))]);
    assert!(!u.waiting_on(&agent), "tried once per change");
    assert!(!u.keep(1, n(TRIES)), "dropped after the last try");
    assert_eq!(u.len(), 0);
}

#[test]
fn helpers_held_stop_at_a_prompt_keeps_its_kind() {
    let mut t = super::table::Table::default();
    t.apply(1, 0, &Event::Started(started(10, "shell", AgentKind::Codex)));
    t.apply(1, 1, &Event::NoticeSuppressed { blocked: false });
    assert!(!t.map[&1].held_blocked);
    t.apply(1, 2, &Event::NoticeSuppressed { blocked: true });
    t.apply(1, 3, &Event::NoticeSuppressed { blocked: false });
    assert!(t.map[&1].held_blocked && t.map[&1].suppressed == 3);
    let l = records::line(5, 2, 1, &Event::NoticeSuppressed { blocked: true });
    assert_eq!(records::parse(&l).map(|p| p.2), Some(Event::NoticeSuppressed { blocked: true }));
    t.apply(1, 4, &Event::BlockedNoticed { prompt: String::new() });
    assert!(!t.map[&1].held_blocked && t.map[&1].suppressed == 0);
}

#[test]
fn helpers_claude_and_kimi_stop_at_an_approval_once() {
    let dir = temp("approval");
    let (s, rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::Started(started(10, "shell", AgentKind::Claude)), Event::TaskDelivered]);
    let mut in_turn = HashMap::new();
    for _ in 0..3 {
        tick::tick(&s, &host_with(&[(1, PaneStatus::Busy(BusyKind::ToolAwaitingApproval))]), &mut in_turn, &mut tick::Parked::default());
    }
    tick::tick(&s, &host_with(&[(1, PaneStatus::AwaitingUser)]), &mut in_turn, &mut tick::Parked::default());
    let stops: Vec<Stop> = rx
        .try_iter()
        .filter_map(|j| match j {
            Job::Stopped { sid: 1, stop, .. } => Some(stop),
            _ => None,
        })
        .collect();
    assert_eq!(stops, vec![Stop::Blocked, Stop::Idle]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_failed_stop_keeps_a_live_helper_and_forgets_a_gone_one() {
    let dir = temp("stop");
    let (s, _rx) = shared_at(&dir);
    // no such session anywhere: gone, so it counts as stopped
    let sid = u64::MAX - 3;
    feed(&s, sid, vec![Event::Started(started(10, "shell", AgentKind::Codex)), Event::TaskDelivered]);
    assert!(stop_all::stop_one(&s, sid, || Err(Reject::new(ErrorCode::Internal, "x"))).is_ok());
    assert!(s.table().map.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_account_change_is_where_it_runs_now() {
    let dir = temp("account");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 4, vec![Event::Started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
    use crate::plugins::claudecode::with_helpers::{AccountChanged, account_changed};
    account_changed(AccountChanged { sid: 4, agent: "claude", from: 2, to: 5, at: 0 });
    account_changed(AccountChanged { sid: 99, agent: "claude", from: 1, to: 3, at: 0 });
    super::work::account_changes(&s);
    assert_eq!(s.table().map[&4].account, "P5");
    assert_eq!(records::replay(&dir.join("events.log"), &|_| None).unwrap().table.map[&4].account, "P5", "and the log says so");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_command_line_words_and_claude_dirs() {
    let dir = Path::new("/st/helpers/42");
    let task = dir.join("task.md");
    let flags = launch::mode_flags(AgentKind::Codex, "-s danger;rm *", dir).unwrap();
    let line = launch::run_script(AgentKind::Codex, &launch::test_account("/Users/x/.codex-profile-2"), Some("gpt 5"), &flags, false, &task).unwrap();
    assert!(line.contains(" -m 'gpt 5' -s 'danger;rm' '*' --add-dir /st/helpers/42 "), "{line}");
    assert_eq!(launch::claude_project_dir("/Users/x/my.repo_v2/a b"), "-Users-x-my-repo-v2-a-b");
    assert_eq!(launch::claude_project_dir("/private/tmp/claude-501/-Users-x"), "-private-tmp-claude-501--Users-x");
}
