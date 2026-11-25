//! What is left when a helper ends, and what a start makes of what went
//! on while nothing ran.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use marspot_peer::control::AgentKind;

use super::pending::Notice;
use super::table::Event;
use super::tests::{feed, shared_at, started, temp};
use super::tests_review::panes;
use super::work::{self, Job};
use super::{close, notify, rebuild, records, tick, timers};
use crate::plugins::TickDue;

fn begun(agent: AgentKind) -> Vec<Event> {
    vec![Event::started(started(10, "shell", agent)), Event::TaskDelivered]
}

fn make(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("task.md"), "x").unwrap();
}

#[test]
fn helpers_an_end_removes_what_an_earlier_one_left() {
    let dir = temp("left");
    let (s, _rx) = shared_at(&dir);
    // no such sessions anywhere
    let (one, two) = (u64::MAX - 11, u64::MAX - 12);
    feed(&s, one, begun(AgentKind::Codex));
    feed(&s, two, begun(AgentKind::Codex));
    feed(&s, 3, begun(AgentKind::Codex));
    for sid in [one, two, 3] {
        make(&s.dir_of(sid));
    }
    std::fs::create_dir_all(dir.join("images")).unwrap();
    assert!(close::close(&s, one) && !s.dir_of(one).exists());
    // its agent, on its way out, wrote its result after all
    make(&s.dir_of(one));
    assert!(close::close(&s, two));
    assert!(!s.dir_of(one).exists() && !s.dir_of(two).exists());
    assert!(s.dir_of(3).exists() && dir.join("images").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_start_removes_the_directory_of_one_that_ended_and_still_runs() {
    let dir = temp("ended");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, begun(AgentKind::Codex));
    feed(&s, 2, [begun(AgentKind::Codex), vec![Event::Stopped { exited: false }]].concat());
    for sid in [1, 2, 3] {
        make(&s.dir_of(sid));
    }
    // 2 was closed, and its session has not gone yet; 3 is gone
    assert_eq!(rebuild::rebuild(&s, || HashSet::from([1, 2])), 1);
    assert!(s.dir_of(1).exists() && !s.dir_of(2).exists() && !s.dir_of(3).exists());
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_one_that_went_with_its_parent_is_still_ticked() {
    let dir = temp("with");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, begun(AgentKind::Codex));
    let (none, gone) = (HashSet::new(), panes(&[]));
    let t = s.table();
    let past = records::now_ms() + tick::START_GRACE_MS + 1_000;
    assert_eq!(tick::due(&t, &none, &tick::Parked::default(), &gone, false, past), TickDue::Event, "its parent asks for none");
    assert_eq!(tick::gone_due(&t, &none, &gone, past), TickDue::Cadence);
    let soon = Instant::now() + Duration::from_secs(61);
    assert!(matches!(tick::gone_due(&t, &none, &gone, records::now_ms()), TickDue::At(at) if at < soon), "not before it may be missed");
    assert_eq!(tick::gone_due(&t, &HashSet::from([1]), &gone, past), TickDue::Event, "being closed");
    assert_eq!(tick::gone_due(&t, &none, &panes(&[(1, 5)]), past), TickDue::Event);
    let (a, b) = (Instant::now(), Instant::now() + Duration::from_secs(1));
    assert_eq!(tick::sooner(TickDue::At(b), TickDue::At(a)), TickDue::At(a));
    assert_eq!(tick::sooner(TickDue::Event, TickDue::At(b)), TickDue::At(b));
    assert_eq!(tick::sooner(TickDue::At(b), TickDue::Cadence), TickDue::Cadence);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_notices_screen_has_no_status_line_of_a_run() {
    let typed = marspot::shell_proto::run_trailer(false);
    let end = notify::screen_end(&format!("$ make\nok\n{typed}\nlast\n\n"), 3);
    assert_eq!(end, "$ make\nok\nlast", "the lines are counted without it");
}

#[test]
fn helpers_a_notice_kept_by_an_earlier_worker_wakes_the_one_that_runs() {
    let dir = temp("wakes");
    let (s, rx) = shared_at(&dir);
    feed(&s, 1, begun(AgentKind::Codex));
    let _ = rx.try_iter().count();
    s.wake_queued.store(false, std::sync::atomic::Ordering::Release);
    let n = Notice { parent: 10, body: "x".into(), tries: 1, ..Notice::default() };
    work::not_delivered(&s, 1, n, "connection refused", &panes(&[(10, 55)]), records::now_ms());
    assert!(matches!(rx.try_recv(), Ok(Job::Wake)));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_redelivery_lost_with_its_queue_is_made_by_the_next_worker() {
    let dir = temp("lost");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, begun(AgentKind::Codex));
    let now = records::now_ms();
    let n = Notice { parent: 10, body: "x".into(), tries: 1, ..Notice::default() };
    work::not_delivered(&s, 1, n, "connection refused", &panes(&[(10, 55)]), now);
    // handed out for a new program, then the worker ended
    assert_eq!(s.lock().undelivered.due(&panes(&[(10, 66)])).len(), 1);
    let epoch = s.arm(std::sync::mpsc::sync_channel(8).0);
    let jobs = timers::due_jobs(&s, epoch, records::now_ms(), |_| true);
    assert!(matches!(jobs.as_slice(), [Job::Redeliver { sid: 1 }]));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_result_written_while_nothing_ran_is_new() {
    let dir = temp("missed");
    let (s, _rx) = shared_at(&dir);
    let told = |text: &str| Event::ResultDelivered { sha: notify::sha(text) };
    // 1 wrote another result, 2 the one told of, 3 its first, 4 none;
    // what was told of 5 is not known
    feed(&s, 1, [begun(AgentKind::Codex), vec![Event::IdleNoticed, told("old")]].concat());
    feed(&s, 2, [begun(AgentKind::Codex), vec![Event::IdleNoticed, told("same")]].concat());
    feed(&s, 3, [begun(AgentKind::Codex), vec![Event::IdleNoticed]].concat());
    feed(&s, 4, begun(AgentKind::Codex));
    feed(&s, 5, [begun(AgentKind::Codex), vec![Event::ResultDelivered { sha: String::new() }]].concat());
    for (sid, text) in [(1, "new"), (2, "same"), (3, "first"), (5, "some")] {
        make(&s.dir_of(sid));
        std::fs::write(s.dir_of(sid).join("result.md"), text).unwrap();
    }
    rebuild::rebuild(&s, || (1..=5).collect());
    let t = s.table();
    let new: Vec<u64> = t.map.values().filter(|h| h.result_new).map(|h| h.sid).collect();
    assert_eq!(new, vec![1, 3]);
    assert!(!t.map[&1].noticed && t.map[&2].noticed);
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}
