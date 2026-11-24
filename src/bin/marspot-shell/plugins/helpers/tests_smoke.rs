use std::collections::HashMap;
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree;
use marspot_peer::control::AgentKind;

use super::notify::Stop;
use super::table::Event;
use super::tests::{feed, host_with, shared_at, started, temp};
use super::work::Job;
use super::{caller_kind, records, service, tick, turn};
use crate::plugins::claudecode::with_helpers::turns::{self, Look};

fn over(at: u64) -> Look {
    Look::Over { began: None, at, quiet_after: None }
}

fn idle_stops(rx: &std::sync::mpsc::Receiver<Job>) -> usize {
    rx.try_iter().filter(|j| matches!(j, Job::Stopped { sid: 1, stop: Stop::Idle, by_screen: false })).count()
}

#[test]
fn helpers_a_recorded_turn_end_is_told_while_the_screen_still_draws() {
    let dir = temp("recorded");
    let (s, rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::Started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
    let (mut in_turn, mut parked) = (HashMap::new(), tick::Parked::default());
    let mut ticks = |status: PaneStatus, n: usize| {
        for _ in 0..n {
            tick::tick(&s, &host_with(&[(1, status.clone())]), &mut in_turn, &mut parked);
        }
        idle_stops(&rx)
    };
    let busy = PaneStatus::Busy(BusyKind::Output);
    // a conversation it goes on with ended a turn before it was started
    turns::publish(vec![(1, over(records::now_ms() - 60_000))]);
    assert_eq!(ticks(busy.clone(), 2), 0);
    turns::publish(Vec::new());
    assert_eq!(ticks(busy.clone(), 2), 0, "in its turn");
    let end = records::now_ms() + 1;
    turns::publish(vec![(1, over(end))]);
    assert_eq!(ticks(busy.clone(), 1), 1, "at the tick after the scan that read it");
    assert_eq!(ticks(busy.clone(), 3) + ticks(PaneStatus::AwaitingUser, 3), 0, "once");
    // a second turn the scans saw running, and a third they did not
    turns::publish(Vec::new());
    assert_eq!(ticks(busy.clone(), 2), 0);
    turns::publish(vec![(1, over(end + 1))]);
    assert_eq!(ticks(busy.clone(), 2), 1);
    turns::publish(vec![(1, over(end + 2))]);
    assert_eq!(ticks(busy.clone(), 2), 1);
    // a turn with no record of its end is still told by the pane's state
    turns::publish(Vec::new());
    assert_eq!(ticks(busy, 2), 0);
    assert_eq!(ticks(PaneStatus::AwaitingUser, 3), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_recorded_turn_end_is_told_once_over_a_restart() {
    let dir = temp("recorded-restart");
    let (s, rx) = shared_at(&dir);
    let begun = vec![Event::Started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered];
    feed(&s, 1, [begun, vec![Event::IdleNoticed, Event::ResultChanged]].concat());
    let told = s.table().map[&1].notices[0];
    for (end, stops) in [(told, 0), (told + 1, 1)] {
        // a start keeps nothing in memory but the table
        s.lock().turn_told.clear();
        let mut in_turn = turn::turns_at_start(&s.table());
        turns::publish(vec![(1, over(end))]);
        for _ in 0..3 {
            tick::tick(&s, &host_with(&[(1, PaneStatus::AwaitingUser)]), &mut in_turn, &mut tick::Parked::default());
        }
        assert_eq!(idle_stops(&rx), stops, "ended {} ms after the notice", end - told);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A claude helper (1) with its pane ticked in one state after another,
/// and a helper of its own (2) when `child`.
struct Ticked {
    s: std::sync::Arc<super::Shared>,
    rx: std::sync::mpsc::Receiver<Job>,
    in_turn: HashMap<u64, turn::Turn>,
    dir: std::path::PathBuf,
}

impl Ticked {
    fn new(tag: &str, child: bool) -> Self {
        let dir = temp(tag);
        let (s, rx) = shared_at(&dir);
        feed(&s, 1, vec![Event::Started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
        if child {
            feed(&s, 2, vec![Event::Started(started(1, "claude", AgentKind::Codex)), Event::TaskDelivered]);
        }
        Ticked { s, rx, in_turn: HashMap::new(), dir }
    }

    /// The stops three ticks with the pane in `status` tell.
    fn told(&mut self, status: PaneStatus) -> usize {
        for _ in 0..3 {
            tick::tick(&self.s, &host_with(&[(1, status.clone())]), &mut self.in_turn, &mut tick::Parked::default());
        }
        idle_stops(&self.rx)
    }

    /// The last helper went as long ago as an end is held after that.
    fn time_passed(&self) {
        *self.s.lock().child_gone.get_mut(&1).unwrap() -= turn::AFTER_HELPERS_MS;
    }
}

impl Drop for Ticked {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const BUSY: PaneStatus = PaneStatus::Busy(BusyKind::Output);

#[test]
fn helpers_a_turn_ended_to_wait_for_its_own_helpers_is_not_told() {
    let mut h = Ticked::new("held", true);
    turns::publish(vec![(1, Look::NotOver)]);
    assert_eq!(h.told(BUSY), 0);
    let end = records::now_ms() + 1;
    turns::publish(vec![(1, over(end))]);
    assert_eq!(h.told(BUSY) + h.told(PaneStatus::AwaitingUser), 0, "its helper runs");
    // nor by the pane's state, once the scans say nothing
    turns::publish(Vec::new());
    assert_eq!(h.told(BUSY) + h.told(PaneStatus::AwaitingUser), 0);
    turns::publish(vec![(1, over(end))]);
    // closed with no notice: the end is told, after a moment
    feed(&h.s, 2, vec![Event::Stopped { exited: false }]);
    assert_eq!(h.told(PaneStatus::AwaitingUser), 0, "only just");
    h.time_passed();
    assert_eq!(h.told(PaneStatus::AwaitingUser), 1);
    turns::publish(Vec::new());
    assert_eq!(h.told(PaneStatus::AwaitingUser), 0, "once");
}

#[test]
fn helpers_a_held_end_with_no_scans_left_is_told_once_its_helpers_go() {
    let mut h = Ticked::new("held-state", true);
    let end = records::now_ms() + 1;
    turns::publish(vec![(1, over(end))]);
    assert_eq!(h.told(BUSY), 0);
    turns::publish(Vec::new());
    assert_eq!(h.told(BUSY) + h.told(PaneStatus::AwaitingUser), 0);
    feed(&h.s, 2, vec![Event::Stopped { exited: false }]);
    h.time_passed();
    assert_eq!(h.told(PaneStatus::AwaitingUser), 1);
    turns::publish(vec![(1, over(end))]);
    assert_eq!(h.told(BUSY), 0, "the same end");
}

#[test]
fn helpers_a_notice_about_its_helper_starts_the_turn_that_is_told() {
    let mut h = Ticked::new("held-notice", true);
    let end = records::now_ms() + 1;
    turns::publish(vec![(1, over(end))]);
    assert_eq!(h.told(BUSY), 0);
    feed(&h.s, 2, vec![Event::IdleNoticed]);
    assert_eq!(h.told(PaneStatus::AwaitingUser), 0, "the notice is on its way");
    turns::publish(vec![(1, Look::NotOver)]);
    assert_eq!(h.told(PaneStatus::AwaitingUser), 0, "seen before its pane draws");
    assert_eq!(h.told(BUSY), 0);
    feed(&h.s, 1, vec![Event::ResultChanged]);
    turns::publish(vec![(1, over(end + 1))]);
    assert_eq!(h.told(BUSY), 1);
}

#[test]
fn helpers_a_new_result_is_told_while_its_helpers_run() {
    let mut h = Ticked::new("held-result", true);
    feed(&h.s, 1, vec![Event::ResultChanged]);
    turns::publish(vec![(1, over(records::now_ms() + 1))]);
    assert_eq!(h.told(BUSY), 1);
}

#[test]
fn helpers_an_end_told_by_the_pane_is_not_told_again_by_its_record() {
    let mut h = Ticked::new("state-first", false);
    let began = Some(records::now_ms());
    turns::publish(vec![(1, Look::NotOver)]);
    assert_eq!(h.told(BUSY), 0);
    assert_eq!(h.told(PaneStatus::AwaitingUser), 1, "its Stop hooks run long");
    let end = records::now_ms() + 1_000;
    turns::publish(vec![(1, Look::Over { began, at: end, quiet_after: None })]);
    assert_eq!(h.told(BUSY), 0, "the same end");
    turns::publish(vec![(1, over(end + 1))]);
    assert_eq!(h.told(BUSY), 1, "a turn no scan saw running");
    // told by the pane again; then a user record, a pane at work, or one
    // that only drew something
    let working = PaneStatus::Busy(BusyKind::Working);
    for (began, then, told) in [(true, None, 1), (false, Some(working), 1), (false, Some(BUSY), 0)] {
        turns::publish(vec![(1, Look::NotOver)]);
        assert_eq!(h.told(BUSY), 0);
        assert_eq!(h.told(PaneStatus::AwaitingUser), 1);
        let at = records::now_ms() + 2_000;
        if let Some(status) = &then {
            assert_eq!(h.told(status.clone()), 0);
        }
        turns::publish(vec![(1, Look::Over { began: began.then_some(at - 1_000), at, quiet_after: None })]);
        assert_eq!(h.told(BUSY), told, "began={began} then={then:?}");
    }
}

/// What asks, when the process asking runs under one whose argv[0] is
/// `name`, with this test standing in for the pane's shell.
fn kind_under(name: &str) -> Option<&'static str> {
    let mut c = Command::new("/bin/sh");
    c.arg0(name).args(["-c", "exec 3<&0; /bin/cat <&3 >/dev/null & exec 3<&-; echo $!; wait"]).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let asker: i32 = line.trim().parse().unwrap();
    let me = std::process::id() as i32;
    let chain = crate::pane_control::caller::chain(asker, pidtree::proc_row, me);
    assert_eq!(chain.iter().map(|r| r.pid).collect::<Vec<_>>(), [asker, child.id() as i32, me]);
    let kind = caller_kind::among(&chain);
    // the asker reads this pipe: closing it ends both
    drop(child.stdin.take());
    child.wait().unwrap();
    kind
}

#[test]
fn helpers_caller_is_the_agent_the_request_came_through() {
    assert_eq!(kind_under("claude"), Some("claude"));
    assert_eq!(kind_under("/opt/homebrew/bin/codex"), Some("codex"));
    assert_eq!(kind_under("sh"), None);
}

#[test]
fn helpers_quiet_is_counted_from_the_pane_s_last_output() {
    let _state = marspot_term::test_env::sandbox_state_dir();
    let sid = 4711;
    assert_eq!(service::quiet_ms(sid, records::now_ms()), None, "no such session");
    std::fs::create_dir_all(marspot_term::session_registry::session_dir(sid)).unwrap();
    let mut log = marspot_term::bytelog::ByteLog::open(sid).unwrap();
    log.append(b"hello").unwrap();
    log.flush();
    let now = records::now_ms();
    let q = service::quiet_ms(sid, now).expect("a live pane's log");
    assert!(q < 30_000, "{q}");
    assert!(service::quiet_ms(sid, now + 90_000).unwrap() >= 90_000);
}
