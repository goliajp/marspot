use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{TrySendError, sync_channel};
use std::time::{Duration, Instant};

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::notify::Stop;
use super::pending::Notice;
use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started, temp};
use super::tests_review::panes;
use super::work::{self, Job};
use super::{Shared, records, tick, timers};
use crate::observe::{Observation, Proc};
use crate::plugins::TickDue;

const NO_INBOX: &str = "proj#1 has no inbox yet: marspot does not know";

fn notice() -> Notice {
    Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed], tries: 1, ..Notice::default() }
}

#[test]
fn helpers_a_held_stop_is_not_released_over_an_undelivered_notice() {
    let dir = temp("release");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "shell", AgentKind::Codex)), Event::TaskDelivered, Event::NoticeSuppressed { blocked: false }]);
    let t0 = records::now_ms();
    assert!(matches!(timers::due_jobs(&s, s.epoch(), t0, |_| true).as_slice(), [Job::Release { sid: 1 }]));
    let agent = panes(&[(10, 55)]);
    work::not_delivered(&s, 1, notice(), NO_INBOX, &agent, t0);
    let mut tried = 0;
    // a worker pass every half second, past the 30 s wait
    for t in (t0..t0 + 60_000).step_by(500) {
        for job in timers::due_jobs(&s, s.epoch(), t, |_| true) {
            assert!(matches!(job, Job::Redeliver { sid: 1 }), "{}", job.name());
            let mut n = s.lock().undelivered.take(1).unwrap();
            n.tries += 1;
            work::not_delivered(&s, 1, n, NO_INBOX, &agent, t);
            tried += 1;
        }
    }
    assert_eq!(tried, 15, "every 2 s for 30 s, then not for a minute");
    let st = s.lock();
    assert!(st.undelivered.holds(1));
    assert_eq!(st.undelivered.next_retry_ms(), Some(t0 + 30_000 + 60_000));
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_refused_send_sets_the_workers_sleep_by_what_is_in_front() {
    let dir = temp("refused");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "shell", AgentKind::Codex))]);
    let now = records::now_ms();
    let (shell, gone, agent) = (panes(&[(10, 0)]), panes(&[]), panes(&[(10, 55)]));
    assert_eq!(timers::sleep_for(&s), None);
    // an agent in front with no inbox yet is tried on the worker's clock
    work::not_delivered(&s, 1, notice(), NO_INBOX, &agent, now);
    assert!(timers::sleep_for(&s).is_some_and(|d| d <= Duration::from_secs(2)));
    assert!(timers::due_jobs(&s, s.epoch(), now + 1_999, |_| true).is_empty());
    assert!(matches!(timers::due_jobs(&s, s.epoch(), now + 2_000, |_| true).as_slice(), [Job::Redeliver { sid: 1 }]));
    // its shell in front, its pane gone, or a refusal of another kind waits longer
    for (obs, err) in [(&shell, NO_INBOX), (&gone, NO_INBOX), (&agent, "connection refused")] {
        work::not_delivered(&s, 1, notice(), err, obs, now);
        assert!(timers::sleep_for(&s).is_some_and(|d| d > Duration::from_secs(50)), "{err}");
        assert!(timers::due_jobs(&s, s.epoch(), now + 59_999, |_| true).is_empty());
        assert!(!s.lock().undelivered.waiting_on(obs), "compared with the same look");
    }
    // an agent that came after the look the refusal was judged by is a change
    work::not_delivered(&s, 1, notice(), NO_INBOX, &shell, now);
    assert!(s.lock().undelivered.waiting_on(&agent));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Wait up to 10 s for `done`.
fn within(done: impl Fn() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while !done() {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn helpers_a_carried_helper_past_its_deadline_is_closed_at_the_start() {
    let dir = temp("begin");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    // no such session anywhere, so its close finds it gone
    let sid = u64::MAX - 5;
    let h = Helper { delivered_ms: Some(1_000), ..Helper::new(sid, started(10, "shell", AgentKind::Claude), 500) };
    records::append(&s.log, records::ROTATE_AT, &s.table(), sid, 1, &Event::Carried(Box::new(h)));
    // the worker may close it before `begin` returns, or once woken by it
    let n = super::rebuild::begin(&s, || HashSet::from([sid]));
    assert_eq!(n.unwrap(), 1, "carried over");
    assert!(within(|| s.table().map.is_empty()), "its budget ran out long ago");
    assert!(records::replay(&s.log, &|_| None).unwrap().table.map.is_empty());
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_job_from_a_thread_of_its_own_waits_for_room() {
    let dir = temp("wait");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    let (tx, rx) = sync_channel(1);
    s.arm(tx);
    assert!(s.queue(Job::Gone { sid: 1 }) && !s.queue(Job::Gone { sid: 2 }), "full");
    let s2 = Arc::clone(&s);
    let t = std::thread::spawn(move || s2.queue_wait(Job::Stopped { sid: 3, stop: Stop::Exited, by_screen: true }));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!t.is_finished());
    // the main loop's send does not wait with it
    assert!(!s.queue(Job::Gone { sid: 4 }));
    assert!(matches!(rx.recv(), Ok(Job::Gone { sid: 1 })));
    t.join().unwrap();
    assert!(matches!(rx.recv(), Ok(Job::Stopped { sid: 3, stop: Stop::Exited, .. })));
    drop(rx);
    // no worker: it returns
    s.queue_wait(Job::Gone { sid: 5 });
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Pane 10 at its shell, with `jobs` under it and `grandchild` under
/// the first.
fn at_shell(jobs: &[i32], grandchild: i32) -> Observation {
    let mut obs = panes(&[(10, 0)]);
    let shell = obs.sessions[0].shell.unwrap().pid;
    let mut rows: Vec<(i32, i32)> = jobs.iter().map(|j| (*j, shell)).collect();
    rows.extend(jobs.first().map(|j| (grandchild, *j)));
    let proc = |(pid, ppid)| Proc { row: crate::observe::tests::row(pid, ppid, "job"), argv: None };
    obs.sessions[0].descendants = rows.into_iter().map(proc).collect();
    obs
}

#[test]
fn helpers_a_parked_parent_is_noted_once_its_jobs_end() {
    let dir = temp("jobs");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "claude", AgentKind::Codex)), Event::TaskDelivered]);
    let none = HashSet::new();
    let t0 = records::now_ms();
    let see = |state: PaneStatus, at: u64, obs: &Observation, p: &mut tick::Parked| tick::reclaim(&s, &host_with(&[(10, state)]), &s.table(), at, obs, p);
    let due = |p: &tick::Parked, obs: &Observation, at: u64| tick::due(&s.table(), &none, p, obs, false, at);
    let (held, free) = (at_shell(&[70], 80), at_shell(&[], 0));
    let late = t0 + 3_600_000;
    for kind in [BusyKind::StoppedJobs, BusyKind::BackgroundJobs] {
        let mut p = tick::Parked::default();
        for t in [t0, t0 + tick::SETTLE_MS, t0 + tick::SETTLE_MS + tick::RECHECK_MS] {
            see(PaneStatus::Busy(kind), t, &held, &mut p);
        }
        assert_eq!(due(&p, &held, late), TickDue::Event);
        assert_eq!(due(&p, &at_shell(&[70], 81), late), TickDue::Event, "what its job starts and ends is no change");
        // the job ended: watched until its state follows, and noted
        assert_eq!(due(&p, &free, late), TickDue::Cadence);
        see(PaneStatus::Busy(BusyKind::Output), late, &free, &mut p);
        assert!(!p.contains(10));
        see(PaneStatus::Busy(BusyKind::Output), late + tick::SETTLE_MS, &free, &mut p);
        assert!(p.contains(10) && matches!(due(&p, &free, late), TickDue::At(_)), "a pane that printed is empty half a minute later");
        see(PaneStatus::Empty, late + tick::SETTLE_MS + tick::RECHECK_MS, &free, &mut p);
        assert!(s.lock().no_agent.remove(&10).is_some());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_parked_parent_is_looked_at_once_more() {
    let dir = temp("recheck");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "claude", AgentKind::Codex)), Event::TaskDelivered]);
    let none = HashSet::new();
    let t0 = records::now_ms();
    let recheck = t0 + tick::SETTLE_MS + tick::RECHECK_MS;
    let shell = panes(&[(10, 0)]);
    let see = |state: PaneStatus, at: u64, p: &mut tick::Parked| tick::reclaim(&s, &host_with(&[(10, state)]), &s.table(), at, &shell, p);
    let due = |p: &tick::Parked, at: u64| tick::due(&s.table(), &none, p, &shell, false, at);
    let odd = PaneStatus::Contradiction { generic: "idle", activity: "awaiting-user" };
    // a contradiction ends with no change under the shell
    let mut p = tick::Parked::default();
    see(odd.clone(), t0, &mut p);
    see(odd.clone(), t0 + tick::SETTLE_MS, &mut p);
    let far = Instant::now() + Duration::from_secs(30);
    assert!(matches!(due(&p, t0 + tick::SETTLE_MS), TickDue::At(at) if at > far));
    see(PaneStatus::Empty, recheck, &mut p);
    assert!(s.lock().no_agent.remove(&10).is_some());
    assert_eq!(due(&p, recheck), TickDue::Cadence, "not noted here, so it is watched");
    // one still printing gets another look; one that is not, no more
    let mut p = tick::Parked::default();
    for t in [t0, t0 + tick::SETTLE_MS, recheck] {
        see(PaneStatus::Busy(BusyKind::Output), t, &mut p);
    }
    assert!(matches!(due(&p, recheck), TickDue::At(_)));
    see(odd, recheck + tick::RECHECK_MS, &mut p);
    assert_eq!(due(&p, recheck + tick::RECHECK_MS), TickDue::Event);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_second_start_takes_over_what_the_first_left() {
    let dir = temp("takeover");
    let (s, _rx) = shared_at(&dir);
    s.lock().no_agent.insert(10, 1);
    work::not_delivered(&s, 9, notice(), NO_INBOX, &panes(&[(10, 55)]), records::now_ms());
    // the first worker took the retry and ended before making it
    assert_eq!(s.lock().undelivered.retries_due(u64::MAX), vec![9]);
    let (tx, _rx2) = sync_channel(8);
    s.arm(tx);
    let st = s.lock();
    assert!(st.no_agent.is_empty());
    assert!(st.undelivered.next_retry_ms().is_some_and(|at| at <= records::now_ms()));
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_second_start_ends_the_first_worker_and_loses_no_fact() {
    let dir = temp("second");
    let s = Arc::new(Shared::at(dir.join("events.log")));
    super::rebuild::begin(&s, HashSet::new).unwrap();
    let first = s.epoch.load(Ordering::Acquire);
    let old = s.jobs.lock().unwrap().clone().unwrap();
    feed(&s, 100, vec![Event::started(started(10, "shell", AgentKind::Claude)), Event::TaskDelivered]);
    // facts written while the table is rebuilt stay in it
    let s2 = Arc::clone(&s);
    let writer = std::thread::spawn(move || {
        for i in 0..400 {
            s2.record(100, Event::AccountChanged(format!("P{i}")));
        }
    });
    assert_eq!(super::rebuild::begin(&s, || HashSet::from([100])).unwrap(), 1);
    writer.join().unwrap();
    assert_eq!(s.table().map[&100].account, "P399");
    assert_eq!(*s.table(), records::replay(&s.log, &|_| None).unwrap().table);
    assert_eq!(s.epoch.load(Ordering::Acquire), first + 1);
    let ended = || matches!(old.try_send(Job::Wake), Err(TrySendError::Disconnected(_)));
    assert!(within(ended), "the first worker ended");
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}
