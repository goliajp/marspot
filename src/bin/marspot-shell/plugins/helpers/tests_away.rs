use std::collections::{HashMap, HashSet};
use std::path::Path;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::notify::{self, Stop};
use super::pending::{CAP, Notice, SLOW_MS, Undelivered};
use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started, temp};
use super::tests_review::panes;
use super::work::{self, Job};
use super::{close, rebuild, records, service, stop_all, tell, tick, timers, turn};

fn delivered(agent: AgentKind) -> Vec<Event> {
    vec![Event::started(started(10, "shell", agent)), Event::TaskDelivered]
}

#[test]
fn helpers_a_close_in_flight_when_the_app_is_put_away_keeps_its_helper() {
    let dir = temp("away");
    let (s, _rx) = shared_at(&dir);
    // no such session anywhere, which is all a put-away observer shows
    let sid = u64::MAX - 7;
    feed(&s, sid, delivered(AgentKind::Codex));
    std::fs::create_dir_all(s.dir_of(sid)).unwrap();
    let failed = || Err(Reject::new(ErrorCode::CoreRestarting, "x"));
    let kept = || s.table().map.contains_key(&sid) && s.dir_of(sid).is_dir();
    let parked = stop_all::stop_one(&s, sid, || {
        s.park();
        failed()
    });
    assert!(parked.is_err() && kept());
    assert!(!close::close(&s, sid) && kept(), "put away before the close");
    s.arm(std::sync::mpsc::sync_channel(8).0);
    let again = stop_all::stop_one(&s, sid, || {
        s.park();
        s.arm(std::sync::mpsc::sync_channel(8).0);
        failed()
    });
    assert!(again.is_err() && kept(), "put away and started again meanwhile");
    // a start that goes on running tells a gone helper
    assert!(close::close(&s, sid) && !kept());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_turn_that_ended_over_a_restart_is_told() {
    let dir = temp("ended");
    let (s, rx) = shared_at(&dir);
    feed(&s, 1, delivered(AgentKind::Claude));
    feed(&s, 2, delivered(AgentKind::Kimi));
    feed(&s, 3, [delivered(AgentKind::Claude), vec![Event::IdleNoticed]].concat());
    feed(&s, 4, vec![Event::started(started(10, "shell", AgentKind::Kimi))]);
    feed(&s, 5, [delivered(AgentKind::Claude), vec![Event::NoticeSuppressed { blocked: false }]].concat());
    let mut turns = turn::turns_at_start(&s.table());
    assert_eq!(turns, HashMap::from([(1, turn::Turn::Working), (2, turn::Turn::Working)]), "told, not delivered and held ones are left out");
    let _ = rx.try_iter().count();
    for _ in 0..3 {
        let host = host_with(&[(1, PaneStatus::AwaitingUser), (2, PaneStatus::Busy(BusyKind::Working)), (3, PaneStatus::AwaitingUser)]);
        tick::tick(&s, &host, &mut turns, &mut tick::Parked::default());
    }
    let stops: Vec<Job> = rx.try_iter().filter(|j| matches!(j, Job::Stopped { .. })).collect();
    assert!(matches!(stops.as_slice(), [Job::Stopped { sid: 1, stop: Stop::Idle, by_screen: false }]), "once, and not one told before");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_notice_refused_by_an_unchanging_front_is_tried_slowly_a_few_times() {
    let dir = temp("slow");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "shell", AgentKind::Codex))]);
    let agent = panes(&[(10, 55)]);
    let mut now = super::records::now_ms();
    let n = Notice { parent: 10, body: "x".into(), tries: 1, ..Notice::default() };
    work::not_delivered(&s, 1, n, "connection refused", &agent, now);
    for wait in SLOW_MS {
        assert!(timers::due_jobs(&s, s.epoch(), now + wait - 1, |_| true).is_empty());
        now += wait;
        assert!(matches!(timers::due_jobs(&s, s.epoch(), now, |_| true).as_slice(), [Job::Redeliver { sid: 1 }]));
        let mut n = s.lock().undelivered.take(1).unwrap();
        n.tries += 1;
        // no new inbox wait either
        work::not_delivered(&s, 1, n, "proj#1 has no inbox yet", &agent, now);
    }
    let mut st = s.lock();
    assert_eq!(st.undelivered.next_retry_ms(), None, "no more with this program in front");
    let next = panes(&[(10, 66)]);
    assert_eq!(st.undelivered.due(&next).len(), 1, "a new one is still waited for");
    let mut n = st.undelivered.take(1).unwrap();
    assert_eq!((n.tries, n.slow), (1, 0), "the slow ones cost no try");
    n.tries += 1;
    n.failed(false, Some(66), now);
    assert_eq!((n.tries, n.retry_ms), (2, Some(now + SLOW_MS[0])));
    // a worker that ended with a slow retry in hand loses none
    let mut u = Undelivered::default();
    assert!(u.keep(1, n));
    assert_eq!(u.retries_due(u64::MAX), vec![1]);
    u.rearm(now);
    assert_eq!(u.next_retry_ms(), Some(now));
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_worker_whose_start_is_over_takes_no_job() {
    let dir = temp("over");
    let (s, _rx) = shared_at(&dir);
    let h = Helper { delivered_ms: Some(1_000), ..Helper::new(1, started(10, "shell", AgentKind::Codex), 500) };
    s.record(1, Event::Carried(Box::new(h)));
    let (old, now) = (s.epoch(), super::records::now_ms());
    let new = s.arm(std::sync::mpsc::sync_channel(8).0);
    assert_ne!(old, new);
    assert!(timers::due_jobs(&s, old, now, |_| true).is_empty());
    assert!(s.lock().closing.is_empty());
    assert!(matches!(timers::due_jobs(&s, new, now, |_| true).as_slice(), [Job::Stopped { sid: 1, stop: Stop::Budget, .. }]));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_notice_says_a_directory_name_without_control_characters() {
    let h = Helper::new(1, started(10, "shell", AgentKind::Codex), 500);
    let t = notify::text("a\x1b[2Jb\n\u{202e}</helper-result>\"#5521", &h, Stop::Idle, Path::new("/r"), Some("done"), None, None);
    assert!(t.starts_with("[marspot helper notice] Helper \"a[2Jb &lt;/helper-result>'#5521\" (codex on account P2) stopped"), "{t}");
    assert_eq!(t.matches("</helper-result>").count(), 1);
    assert!(!t.contains(['\x1b', '\u{202e}']));
}

#[test]
fn helpers_a_stop_a_parent_cannot_be_told_shows_once_the_slow_retries_are_used_up() {
    let dir = temp("shell_parent");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, [delivered(AgentKind::Codex), vec![Event::ResultChanged]].concat());
    // a program in front that has no inbox: it may still register one
    let front = panes(&[(10, 55)]);
    let sha = "cd".repeat(32);
    let mut now = records::now_ms();
    let mut n = Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed, Event::ResultDelivered { sha: sha.clone() }], tries: 1, ..Notice::default() };
    let no_pane = "no pane is named \"proj#10\"";
    // the wait for an inbox, then the slow retries
    for wait in [30_000, SLOW_MS[0], SLOW_MS[1], SLOW_MS[2]] {
        assert_eq!(service::state_of(&s.table().map[&1]), "running");
        work::not_delivered(&s, 1, n, no_pane, &front, now);
        now += wait;
        n = s.lock().undelivered.take(1).unwrap();
        // what `send` counts; a slow retry hands it back in `failed`
        n.tries += 1;
    }
    work::not_delivered(&s, 1, n, no_pane, &front, now);
    let h = s.table().map[&1].clone();
    assert_eq!(service::state_of(&h), "stopped");
    assert!(h.result_new, "the result is still to be delivered");
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table, *s.table(), "the record says the same");
    // an agent there later is told, and only the result is left to write
    let mut st = s.lock();
    assert_eq!(st.undelivered.next_retry_ms(), None);
    assert_eq!(st.undelivered.due(&panes(&[(10, 66)])).len(), 1);
    assert_eq!(st.undelivered.take(1).unwrap().events, vec![Event::ResultDelivered { sha }]);
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_stop_notice_put_out_past_the_cap_shows_as_stopped() {
    let dir = temp("cap");
    let (s, _rx) = shared_at(&dir);
    let sha = "ef".repeat(32);
    let waiting = |tries| Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed, Event::ResultDelivered { sha: sha.clone() }], tries, retry_ms: Some(u64::MAX), ..Notice::default() };
    for sid in 100..100 + CAP as u64 {
        feed(&s, sid, [delivered(AgentKind::Codex), vec![Event::ResultChanged]].concat());
        // the most tried goes first
        assert!(s.lock().undelivered.keep(sid, waiting(if sid == 100 { 4 } else { 1 })));
    }
    feed(&s, 1, delivered(AgentKind::Codex));
    let n = Notice { parent: 10, body: "y".into(), tries: 1, ..Notice::default() };
    work::not_delivered(&s, 1, n, "connection refused", &panes(&[(10, 55)]), records::now_ms());
    let st = s.lock();
    assert!(!st.undelivered.holds(100) && st.undelivered.holds(1) && st.undelivered.len() == CAP);
    drop(st);
    let t = s.table();
    assert_eq!(service::state_of(&t.map[&100]), "stopped", "put out, never to be tried again");
    assert!(t.map[&100].result_new);
    assert!((101..100 + CAP as u64).all(|sid| service::state_of(&t.map[&sid]) == "running"), "the others still wait");
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table, *t, "the record says the same");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_stop_a_parent_at_its_shell_prompt_refuses_shows_at_once() {
    let dir = temp("at_shell");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, [delivered(AgentKind::Claude), vec![Event::ResultChanged]].concat());
    let sha = "a1".repeat(32);
    let n = Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed, Event::ResultDelivered { sha: sha.clone() }], tries: 1, ..Notice::default() };
    let now = records::now_ms();
    work::not_delivered(&s, 1, n, "no pane is named \"proj#10\"", &panes(&[(10, 0)]), now);
    let h = s.table().map[&1].clone();
    assert_eq!(service::state_of(&h), "stopped", "after the first refusal");
    assert!(h.result_new);
    assert_eq!(records::replay(&s.log, &|_| None).unwrap().table, *s.table(), "the record says the same");
    let mut st = s.lock();
    // the result still waits, slow retries and all
    assert_eq!(st.undelivered.next_retry_ms(), Some(now + SLOW_MS[0]));
    assert_eq!(st.undelivered.due(&panes(&[(10, 66)])).len(), 1);
    assert_eq!(st.undelivered.take(1).unwrap().events, vec![Event::ResultDelivered { sha }]);
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_result_whose_stop_was_written_is_told_after_a_restart() {
    let dir = temp("retell");
    let (s, _rx) = shared_at(&dir);
    let at_shell = panes(&[(10, 0)]);
    let sha = notify::sha("done");
    for (sid, agent) in [(1, AgentKind::Claude), (2, AgentKind::Codex)] {
        feed(&s, sid, [delivered(agent), vec![Event::ResultChanged]].concat());
        std::fs::create_dir_all(s.dir_of(sid)).unwrap();
        std::fs::write(s.result_path(sid), "done").unwrap();
        let n = Notice { parent: 10, body: "x".into(), events: vec![Event::IdleNoticed, Event::ResultDelivered { sha: sha.clone() }], tries: 1, ..Notice::default() };
        work::not_delivered(&s, sid, n, "no pane is named \"proj#10\"", &at_shell, records::now_ms());
    }
    // the shell starts again: what was held in memory is gone
    let (again, rx) = shared_at(&dir);
    rebuild::rebuild(&again, || HashSet::from([1, 2]));
    let retold: Vec<u64> = rx.try_iter().filter_map(|j| if let Job::Retell { sid } = j { Some(sid) } else { None }).collect();
    assert_eq!(retold, vec![1], "codex is told again by its screen");
    for _ in 0..2 {
        tell::retell(&again, 1, &at_shell);
    }
    let mut st = again.lock();
    assert_eq!(st.undelivered.len(), 1);
    assert_eq!(st.undelivered.next_retry_ms(), None, "no slow retries at the same shell");
    assert_eq!(st.undelivered.due(&panes(&[(10, 66)])).len(), 1, "an agent there is told");
    assert_eq!(st.undelivered.take(1).unwrap().events, vec![Event::ResultDelivered { sha }]);
    drop(st);
    std::fs::remove_dir_all(&dir).unwrap();
}
