//! Waits against a core the test plays: each frame a connection queues
//! for the main loop is answered through `links`, the way the core's
//! reader thread hands answers over.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use marspot::shell_proto::{
    CAP_TYPE_AND_WATCH, DoneCode, MsgType, PaneDone, PaneTyped, PaneWatchFired, TypedResult, WatchBody, WatchKind,
    WatchReason, decode_pane_type, decode_pane_watch, encode_pane_done, encode_pane_typed, encode_pane_watch_fired,
};
use marspot_peer::control::ErrorCode;

use super::super::caller::{CallerKey, Peer};
use super::super::conn::{Conn, done_reject};
use super::super::{Hub, MainCmd, Reply, hangup};
use super::{hub_at, temp};

fn done(hub: &Hub, req_id: u32, code: DoneCode, text: &str) {
    hub.links.route(MsgType::PaneDone, &encode_pane_done(&PaneDone { req_id, code, text: text.into() }).unwrap());
}

fn fire(hub: &Hub, sid: u64, watch_id: u32, reason: WatchReason, pos: u64) {
    let f = PaneWatchFired { sid, watch_id, reason, pos, status: None, line: None };
    hub.links.route(MsgType::PaneWatchFired, &encode_pane_watch_fired(&f));
}

fn change() -> WatchBody {
    WatchBody { watch_id: 0, kind: WatchKind::Change, since_pos: 0, quiet_ms: 0, text: String::new() }
}

/// Run `body` while `answer` plays the core for every frame queued.
fn with_core<R>(hub: &Hub, rx: Receiver<MainCmd>, answer: impl Fn(&Hub, MsgType, &[u8]) + Send + Sync, body: impl FnOnce() -> R) -> R {
    hub.caps.store(CAP_TYPE_AND_WATCH, Ordering::Release);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let stop = &stop;
        let answer = &answer;
        s.spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let Ok(MainCmd::Send { msg, payload, .. }) = rx.recv_timeout(Duration::from_millis(20)) {
                    answer(hub, msg, &payload);
                }
            }
        });
        let out = body();
        stop.store(true, Ordering::Release);
        out
    })
}

/// Registrations only; cancels are not answered.
fn registration(msg: MsgType, p: &[u8]) -> Option<marspot::shell_proto::PaneWatch> {
    let w = (msg == MsgType::PaneWatch).then(|| decode_pane_watch(p).ok()).flatten()?;
    (w.watch.kind != WatchKind::Cancel).then_some(w)
}

#[test]
fn pane_control_restarting_answer_is_retryable() {
    let r = done_reject(DoneCode::Other, "restarting");
    assert_eq!(r.code, ErrorCode::CoreRestarting);
    assert!(r.retry_after_ms.is_some());
    assert_eq!(done_reject(DoneCode::Other, "empty text").code, ErrorCode::Internal);
}

#[test]
fn pane_control_wait_registers_again_after_restarting() {
    let (hub, rx) = hub_at(&temp("restart"));
    let n = AtomicU32::new(0);
    let out = with_core(
        &hub,
        rx,
        |hub, msg, p| {
            let Some(w) = registration(msg, p) else { return };
            match n.fetch_add(1, Ordering::AcqRel) {
                0 => {
                    done(hub, w.req_id, DoneCode::Ok, "");
                    fire(hub, w.sid, w.watch.watch_id, WatchReason::Restarting, 5);
                }
                // the old image, on its way out
                1 => done(hub, w.req_id, DoneCode::Other, "restarting"),
                _ => {
                    assert_eq!(w.watch.since_pos, 5, "the same start again");
                    done(hub, w.req_id, DoneCode::Ok, "");
                    fire(hub, w.sid, w.watch.watch_id, WatchReason::Met, 9);
                }
            }
        },
        || {
            let mut conn = Conn::new(&hub, Peer::default(), None);
            conn.alive = |_| true;
            conn.wait(7, CallerKey::Pane(1), change(), Some(5), Instant::now() + Duration::from_secs(10))
        },
    );
    let f = out.unwrap().unwrap();
    assert_eq!((f.reason, f.pos), (WatchReason::Met, 9));
    assert_eq!(hub.links.sizes().1, 0);
}

fn restart_once_then(answer_later: fn(&Hub, u32)) -> impl Fn(&Hub, MsgType, &[u8]) + Send + Sync {
    let first = AtomicBool::new(true);
    move |hub, msg, p| {
        let Some(w) = registration(msg, p) else { return };
        if first.swap(false, Ordering::AcqRel) {
            done(hub, w.req_id, DoneCode::Ok, "");
            fire(hub, w.sid, w.watch.watch_id, WatchReason::Restarting, 5);
        } else {
            answer_later(hub, w.req_id);
        }
    }
}

#[test]
fn pane_control_wait_ends_when_session_is_gone_while_restarting() {
    let (hub, rx) = hub_at(&temp("gone"));
    let t = Instant::now();
    let out = with_core(&hub, rx, restart_once_then(|hub, id| done(hub, id, DoneCode::Other, "restarting")), || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        conn.alive = |_| false;
        conn.wait(7, CallerKey::Pane(1), change(), Some(5), Instant::now() + Duration::from_secs(60))
    });
    assert_eq!(out.unwrap().unwrap().reason, WatchReason::SessionEnded);
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn pane_control_reregistering_is_capped() {
    let (hub, rx) = hub_at(&temp("cap"));
    let t = Instant::now();
    let out = with_core(&hub, rx, restart_once_then(|hub, id| done(hub, id, DoneCode::Other, "restarting")), || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        conn.alive = |_| true;
        conn.regrace = Duration::from_millis(600);
        conn.wait(7, CallerKey::Pane(1), change(), Some(5), Instant::now() + Duration::from_secs(60))
    });
    assert_eq!(out.unwrap().unwrap().reason, WatchReason::Restarting);
    let took = t.elapsed();
    // given up at the last round that fits in the 600 ms
    assert!(took >= Duration::from_millis(350) && took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn pane_control_caller_hanging_up_ends_its_wait() {
    let (hub, rx) = hub_at(&temp("hangup"));
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let t = Instant::now();
    let out = with_core(
        &hub,
        rx,
        |hub, msg, p| {
            if let Some(w) = registration(msg, p) {
                done(hub, w.req_id, DoneCode::Ok, "");
            }
        },
        || {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                drop(theirs);
            });
            let mut conn = Conn::new(&hub, Peer::default(), Some(ours.as_raw_fd()));
            conn.wait(7, CallerKey::Pane(1), change(), Some(5), Instant::now() + Duration::from_secs(60))
        },
    );
    assert!(out.is_err(), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(hub.links.sizes().1, 0, "the wait's slot is given back");
}

#[test]
fn pane_control_half_closed_caller_is_not_hung_up() {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _armed = hangup::watch(ours.as_raw_fd(), tx).unwrap();
    theirs.shutdown(std::net::Shutdown::Write).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "a caller still reading is not gone");
    drop(theirs);
    assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Reply::HungUp)));
}

#[test]
fn pane_control_hangup_of_an_ended_watch_does_not_reach_the_fd_s_next_owner() {
    let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd = ours.as_raw_fd();
    let (old_tx, _old_rx) = std::sync::mpsc::channel();
    let old = hangup::watch(fd, old_tx).unwrap();
    let stale = old.token();
    drop(old);
    // the same fd number, watched again for another connection
    let (tx, rx) = std::sync::mpsc::channel();
    let now = hangup::watch(fd, tx).unwrap();
    assert!(!hangup::deliver_for_test(fd, stale));
    assert!(rx.try_recv().is_err());
    assert!(hangup::deliver_for_test(fd, now.token()));
    assert!(matches!(rx.try_recv(), Ok(Reply::HungUp)));
}

#[test]
fn pane_control_late_reports_are_kept_only_for_live_watches() {
    let (hub, rx) = hub_at(&temp("late"));
    let typed = |hub: &Hub, msg: MsgType, p: &[u8]| {
        if msg == MsgType::PaneType {
            let t = decode_pane_type(p).unwrap();
            let r = PaneTyped { req_id: t.req_id, sid: t.sid, result: TypedResult::Written, echo_off: false, pos: 3, written: 0 };
            hub.links.route(MsgType::PaneTyped, &encode_pane_typed(&r));
        }
    };
    with_core(&hub, rx, typed, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        let gone = conn.add_watch(7, CallerKey::Pane(1), change()).unwrap();
        let kept = conn.add_watch(7, CallerKey::Pane(1), change()).unwrap();
        hub.links.drop_watch(gone.watch_id);
        for id in [gone.watch_id, kept.watch_id] {
            let f = PaneWatchFired { sid: 7, watch_id: id, reason: WatchReason::Met, pos: 1, status: None, line: None };
            conn.tx.send(Reply::Fired(f)).unwrap();
        }
        assert_eq!(conn.probe_pos(7, Duration::from_secs(3)), Some(3));
        assert_eq!(conn.fired.iter().map(|f| f.watch_id).collect::<Vec<_>>(), vec![kept.watch_id]);
    });
}

#[test]
fn pane_control_resync_skips_waits_not_yet_counting() {
    let (hub, _rx) = hub_at(&temp("resync"));
    let (tx, _r) = std::sync::mpsc::channel();
    let id = hub.links.add_watch(7, CallerKey::Pane(1), change(), tx, (8, 64)).unwrap();
    assert!(hub.links.watch_bodies().is_empty(), "a wait from now is its connection's to register");
    hub.links.set_since(id, 42);
    let bodies = hub.links.watch_bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!((bodies[0].0, bodies[0].1.since_pos, bodies[0].1.watch_id), (7, 42, id));
}
