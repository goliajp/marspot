use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::notify::Stop;
use super::pending::{self, Notice, Undelivered};
use super::table::Event;
use super::tests::{feed, host_with, started, temp};
use super::tests_review::panes;
use super::work::Job;
use super::{Shared, tick};

#[test]
fn helpers_a_parent_agent_without_an_inbox_yet_is_tried_soon_and_costs_no_try() {
    assert!(pending::not_ready("proj#7359 has no inbox yet: marspot does not know"));
    assert!(pending::not_ready("no pane is named \"gds#2000\""));
    assert!(!pending::not_ready("connection refused"));
    let t0 = 1_000_000;
    // the first send went (tries 1) and the agent has not registered
    let mut n = Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed], tries: 1, ..Notice::default() };
    n.failed(true, Some(55), t0);
    assert_eq!((n.tries, n.retry_ms, n.settle_until), (0, Some(t0 + 2_000), Some(t0 + 30_000)));
    let mut u = Undelivered::default();
    assert!(u.keep(1, n));
    assert_eq!(u.next_retry_ms(), Some(t0 + 2_000));
    assert!(u.retries_due(t0 + 1_999).is_empty());
    assert_eq!(u.retries_due(t0 + 2_000), vec![1]);
    assert!(u.retries_due(t0 + 2_000).is_empty(), "handed out once");
    assert_eq!(u.next_retry_ms(), None);
    // tried again near the end of the wait: the last retry is at its end
    let mut n = u.take(1).unwrap();
    n.tries += 1;
    n.failed(true, Some(55), t0 + 29_000);
    assert_eq!((n.tries, n.retry_ms), (0, Some(t0 + 30_000)));
    // at the end it counts, and the slow retries follow
    n.tries += 1;
    n.failed(true, Some(55), t0 + 30_000);
    assert_eq!((n.tries, n.retry_ms, n.settle_until), (1, Some(t0 + 90_000), None));
    // a refusal of another kind counts at once
    let mut m = Notice { parent: 10, tries: 1, ..Notice::default() };
    m.failed(false, Some(0), t0);
    assert_eq!((m.tries, m.retry_ms, m.front), (1, Some(t0 + 60_000), Some(0)));
}

#[test]
fn helpers_a_new_program_starts_a_new_wait_and_a_redelivery_not_queued_is_seen_again() {
    let mut u = Undelivered::default();
    let mut n = Notice { parent: 10, tries: 1, ..Notice::default() };
    n.failed(true, Some(55), 0);
    n.failed(true, Some(55), 40_000);
    assert_eq!(n.settle_until, None, "the wait ran out");
    assert!(u.keep(1, n));
    let agent = panes(&[(10, 66)]);
    assert_eq!(u.due(&agent), vec![(1, Some(55))]);
    assert!(!u.waiting_on(&agent));
    u.rollback(1, Some(55));
    assert!(u.waiting_on(&agent), "the change is pending again");
    assert_eq!(u.due(&agent), vec![(1, Some(55))]);
}

#[test]
fn helpers_a_stop_not_queued_is_queued_on_the_next_tick() {
    let dir = temp("full");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    s.arm(tx);
    feed(&s, 1, vec![Event::started(started(10, "shell", AgentKind::Claude)), Event::TaskDelivered]);
    let mut in_turn = HashMap::new();
    let mut parked = tick::Parked::default();
    let _ = rx.try_iter().count();
    for (status, stop) in [(PaneStatus::Busy(BusyKind::ToolAwaitingApproval), Stop::Blocked), (PaneStatus::AwaitingUser, Stop::Idle)] {
        tick::tick(&s, &host_with(&[(1, PaneStatus::Busy(BusyKind::Working))]), &mut in_turn, &mut parked);
        assert!(s.queue(Job::Wake), "the queue is full now");
        tick::tick(&s, &host_with(&[(1, status.clone())]), &mut in_turn, &mut parked);
        assert!(matches!(rx.try_recv(), Ok(Job::Wake)) && rx.try_recv().is_err(), "{status:?}: not queued");
        tick::tick(&s, &host_with(&[(1, status.clone())]), &mut in_turn, &mut parked);
        assert!(matches!(rx.try_recv(), Ok(Job::Stopped { sid: 1, stop: got, .. }) if got == stop), "{status:?}: queued the next time");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_second_start_queues_to_the_new_worker() {
    let dir = temp("rearm");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    let (tx1, rx1) = std::sync::mpsc::sync_channel(8);
    s.arm(tx1);
    s.lock().closing.extend([3, 4]);
    s.lock().close_retry.insert(4, (u64::MAX, 2));
    s.park();
    assert!(s.parked.load(Ordering::Acquire) && s.epoch() == 2);
    let _ = rx1.try_iter().count();
    let (tx2, rx2) = std::sync::mpsc::sync_channel(8);
    s.arm(tx2);
    assert!(!s.parked.load(Ordering::Acquire));
    s.wake();
    assert!(rx1.try_recv().is_err() && matches!(rx2.try_recv(), Ok(Job::Wake)));
    let st = s.lock();
    assert!(!st.closing.contains(&3), "its close went with the old queue");
    assert!(st.closing.contains(&4) && st.close_retry[&4].0 <= super::records::now_ms(), "a close to retry is due now");
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}
