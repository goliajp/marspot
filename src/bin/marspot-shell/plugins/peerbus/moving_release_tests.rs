//! A move's mark and the wait for the old home, against a real worker,
//! a real stand-in TUI process and a stand-in daemon (see `moving_tests`).

use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::json::{obj, Value};

use super::fake_daemon::thread;
use super::moving::{self, Ask, Held, Verdict, MARK_FOR, RELEASE_WAIT};
use super::moving_checks::sqlite3_with;
use super::moving_tests::{bind_as, rig, tui, Rig, SID, T};
use super::worker::{Event, Worker};
use crate::plugins::pty_op::Job;

const CHILD: &str = "01a133f4-0000-7331-9630-9c40ff4b467f";

fn released(r: &mut Rig) -> Arc<Job> {
    match r.verdict() {
        Verdict::Go { released, .. } => released.0,
        v => panic!("{v:?}"),
    }
}

/// The switch op has ended the TUI.
fn terminated(r: &mut Rig) {
    r.w.on_move(Ask::Terminated { sid: SID, codex: r.codex });
}

/// Make the worker look at the old home now.
fn look(r: &mut Rig) {
    let now = Instant::now();
    r.w.moves.releases.values_mut().for_each(|x| x.next = Some(now));
    r.w.moves_due(now);
}

/// A status change the daemon sends for `thread`.
fn status(thread: &str, kind: &str) -> Value {
    obj([("method", Value::from("thread/status/changed")), ("params", obj([("threadId", Value::from(thread)), ("status", obj([("type", Value::from(kind))]))]))])
}

fn end_tui(r: &mut Rig) {
    let _ = r.tui.kill();
    let _ = r.tui.wait();
}

#[test]
fn the_old_home_is_unsubscribed_the_moment_a_move_goes() {
    let mut r = rig();
    // not subscribed by the worker: the TUI's own load is what to undo
    r.w.bindings.get_mut(&SID).unwrap().subscribed = false;
    let _ = released(&mut r);
    assert_eq!(r.daemon.asked("thread/unsubscribe"), 1);
}

#[test]
fn the_switch_goes_on_only_once_the_old_home_has_let_go() {
    let mut r = rig();
    let job = released(&mut r);
    terminated(&mut r);
    assert!(r.w.moves.due().is_some(), "the worker wakes for it");
    look(&mut r);
    assert_eq!(job.poll(), None, "still loaded there");
    r.daemon.state.lock().unwrap().threads.clear();
    look(&mut r);
    assert_eq!(job.poll(), Some(Ok(())));
    assert!(r.w.moves.releases.is_empty());
}

#[test]
fn an_old_home_that_keeps_the_session_fails_the_switch_at_the_deadline() {
    let mut r = rig();
    let job = released(&mut r);
    terminated(&mut r);
    r.w.moves.releases.get_mut(&SID).unwrap().until = Some(Instant::now());
    look(&mut r);
    assert!(matches!(job.poll(), Some(Err(_))), "{:?}", job.poll());
    assert!(r.w.moves.releases.is_empty());
}

#[test]
fn an_end_while_waiting_drops_the_wait() {
    let mut r = rig();
    let job = released(&mut r);
    r.w.on_move(Ask::End { sid: SID, codex: r.codex });
    assert!(matches!(job.poll(), Some(Err(_))));
    assert!(r.w.moves.releases.is_empty());
}

#[test]
fn an_end_after_the_tui_is_gone_leaves_the_mark_to_its_binding() {
    let mut r = rig();
    let _ = released(&mut r);
    end_tui(&mut r);
    r.w.on_move(Ask::End { sid: SID, codex: r.codex });
    assert!(r.w.moving(SID), "the binding still points at the old home");
    assert_eq!(r.message("m1").and_then(|v| v.str_at("status_detail").map(str::to_string)).as_deref(), Some("refused"));
    let b = r.w.bindings.remove(&SID).unwrap();
    r.w.unbind(SID, b);
    assert!(!r.w.moving(SID));
}

#[test]
fn a_new_thread_in_the_same_tui_does_not_clear_the_mark() {
    let mut r = rig();
    let _ = released(&mut r);
    // `/new`: the binding goes, the process stays
    let b = r.w.bindings.remove(&SID).unwrap();
    r.w.unbind(SID, b);
    assert!(r.w.moving(SID));
}

#[test]
fn a_mark_whose_move_never_happened_expires() {
    let mut r = rig();
    let job = released(&mut r);
    let resumed = r.daemon.asked("thread/resume");
    let past = Instant::now() - Duration::from_secs(121);
    r.w.moves.marked.get_mut(&SID).unwrap().at = past;
    r.w.moves_due(Instant::now());
    assert!(!r.w.moving(SID));
    assert_eq!(r.daemon.asked("thread/resume"), resumed + 1, "subscribed again");
    assert!(matches!(job.poll(), Some(Err(_))), "nothing waits for it any more");
}

#[test]
fn the_wait_for_the_old_home_starts_when_the_tui_is_gone() {
    let mut r = rig();
    let _ = released(&mut r);
    assert_eq!(r.w.moves.releases[&SID].until, None, "no clock while the TUI is up");
    assert_eq!(r.w.moves.due().map(|d| d > Instant::now() + MARK_FOR / 2), Some(true), "only the mark's");
    let before = Instant::now();
    terminated(&mut r);
    let until = r.w.moves.releases[&SID].until.expect("the clock runs");
    assert!(until >= before + RELEASE_WAIT, "{:?}", until - before);
}

#[test]
fn a_switch_op_waiting_its_turn_keeps_the_mark() {
    let mut r = rig();
    let job = released(&mut r);
    r.w.on_move(Ask::Submitted { sid: SID, codex: r.codex });
    r.w.moves.marked.get_mut(&SID).unwrap().at = Instant::now() - MARK_FOR - Duration::from_secs(60);
    r.w.moves_due(Instant::now());
    assert!(r.w.moving(SID), "the op ends with an end or the move");
    assert_eq!(job.poll(), None);
    assert_eq!(r.w.moves.due(), None, "nothing to wake for until the TUI is gone");
}

#[test]
fn an_idle_notice_during_a_move_does_not_subscribe_the_old_home_again() {
    let mut r = rig();
    let _ = released(&mut r);
    let resumed = r.daemon.asked("thread/resume");
    let home = r.home.clone();
    r.w.on_note(&home, &status(T, "idle"));
    assert_eq!(r.daemon.asked("thread/resume"), resumed);
    assert!(!r.w.bindings[&SID].subscribed);
}

#[test]
fn a_steady_stream_of_events_does_not_hold_off_the_wait() {
    let mut r = rig();
    let job = released(&mut r);
    terminated(&mut r);
    // another pane's session, each of whose idle notices costs the
    // worker two slow calls: events come faster than they are handled
    const T2: &str = "01a133f4-1111-7331-9630-9c40ff4b467f";
    let (home, cwd) = (r.home.clone(), r.cwd.clone());
    r.daemon.state.lock().unwrap().threads.push((T2.into(), thread(&cwd, None, "idle")));
    let (mut other, codex2) = tui(&home, &cwd);
    bind_as(&mut r.w, SID + 1, T2, &home, &cwd, codex2);
    r.w.bindings.get_mut(&(SID + 1)).unwrap().subscribed = false;
    {
        let mut s = r.daemon.state.lock().unwrap();
        s.threads.retain(|(id, _)| id != T);
        s.refused.push("thread/resume");
        s.delay = Duration::from_millis(100);
    }
    let tx = r.w.tx.clone();
    let (stand_tx, stand_rx) = sync_channel(1);
    let mut w = std::mem::replace(&mut r.w, Worker::new(stand_tx, stand_rx, String::new()));
    let worker = std::thread::spawn(move || {
        w.serve();
        w
    });
    let note = status(T2, "idle");
    let start = Instant::now();
    while job.poll().is_none() && start.elapsed() < Duration::from_secs(10) {
        tx.send(Event::Rpc { home: home.clone(), msg: note.clone() }).unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
    let took = start.elapsed();
    r.daemon.state.lock().unwrap().delay = Duration::ZERO;
    tx.send(Event::Stop).unwrap();
    r.w = worker.join().unwrap();
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(job.poll(), Some(Ok(())), "after {took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn an_end_gets_through_a_full_channel() {
    let mut r = rig();
    let _ = released(&mut r);
    let (tx, _rx) = sync_channel(1);
    tx.send(Event::Names).unwrap();
    moving::connect(Some(tx));
    assert!(moving::ask(Ask::End { sid: SID, codex: r.codex }));
    r.w.take_ends();
    moving::connect(None);
    assert!(!r.w.moving(SID));
}

#[test]
fn a_loaded_thread_that_cannot_be_read_holds_the_move() {
    let mut r = rig();
    r.daemon.state.lock().unwrap().unreadable.push(CHILD.into());
    assert_eq!(r.verdict(), Verdict::Held(Held::Unknown));
}

#[test]
fn a_child_on_a_later_page_holds_the_move() {
    let mut r = rig();
    {
        let mut s = r.daemon.state.lock().unwrap();
        s.threads.push((CHILD.into(), thread(&r.cwd, Some(T), "active")));
        s.page = Some(1);
    }
    assert_eq!(r.verdict(), Verdict::Held(Held::ChildRunning));
}

#[test]
fn a_stuck_sqlite3_is_given_up_on() {
    let r = rig();
    let start = Instant::now();
    let out = sqlite3_with("/bin/sh", &["-c", "sleep 30", "sh"], &r.home.join("q.sqlite"), "select 1;", Duration::from_millis(300));
    assert!(out.is_err(), "{out:?}");
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
}
