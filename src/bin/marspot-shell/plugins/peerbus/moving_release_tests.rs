//! A move's mark and the wait for the old home, against a real worker,
//! a real stand-in TUI process and a stand-in daemon (see `moving_tests`).

use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::fake_daemon::thread;
use super::moving::{self, Ask, Held, Verdict};
use super::moving_checks::sqlite3_with;
use super::moving_tests::{rig, Rig, SID, T};
use super::worker::Event;
use crate::plugins::pty_op::Job;

const CHILD: &str = "01a133f4-0000-7331-9630-9c40ff4b467f";

fn released(r: &mut Rig) -> Arc<Job> {
    match r.verdict() {
        Verdict::Go { released, .. } => released.0,
        v => panic!("{v:?}"),
    }
}

/// Make the worker look at the old home now.
fn look(r: &mut Rig) {
    let now = Instant::now();
    r.w.moves.releases.values_mut().for_each(|x| x.next = now);
    r.w.moves_due(now);
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
    r.w.moves.releases.get_mut(&SID).unwrap().until = Instant::now();
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
    let _ = released(&mut r);
    let resumed = r.daemon.asked("thread/resume");
    let past = Instant::now() - Duration::from_secs(121);
    r.w.moves.marked.get_mut(&SID).unwrap().at = past;
    r.w.moves_due(Instant::now());
    assert!(!r.w.moving(SID));
    assert_eq!(r.daemon.asked("thread/resume"), resumed + 1, "subscribed again");
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
