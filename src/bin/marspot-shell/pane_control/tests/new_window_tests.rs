//! `pane.open` with `new_window`: the core is asked for a pane in a new
//! window with no parent, and the answer names that window.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use marspot::shell_proto::{CAP_NEW_WINDOW, CAP_SHADOWS, MsgType, PaneSpawned, decode_pane_spawn, encode_pane_spawned};
use marspot_peer::control::{ErrorCode, OpenArgs};

use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::Conn;
use super::super::{MainCmd, ops_pane};
use super::{ctx, entry, hub_at, temp};

fn new_window() -> OpenArgs {
    OpenArgs { cwd: None, shadow: false, new_window: true, parent: None, cols: 120, rows: 40, command: None }
}

#[test]
fn pane_control_open_in_a_new_window_has_no_parent_and_names_its_window() {
    let (hub, rx) = hub_at(&temp("newwin"));
    hub.caps.store(CAP_SHADOWS | CAP_NEW_WINDOW, Ordering::Release);
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w")], Config::default());
    let h = &hub;
    let (m, out) = std::thread::scope(|s| {
        let core = s.spawn(move || {
            let Ok(MainCmd::Send { msg: MsgType::PaneSpawn, payload, .. }) = rx.recv_timeout(Duration::from_secs(5)) else {
                panic!("no spawn was sent")
            };
            let m = decode_pane_spawn(&payload).unwrap();
            let s = PaneSpawned { req_id: m.req_id, ok: true, sid: 77, err: String::new(), window_id: 9 };
            h.links.route(MsgType::PaneSpawned, &encode_pane_spawned(&s).unwrap());
            m
        });
        let mut conn = Conn::new(&hub, Peer::default(), None);
        let out = ops_pane::open(&mut conn, &c, &new_window()).unwrap();
        (core.join().unwrap(), out)
    });
    assert!(m.new_window && !m.shadow, "{m:?}");
    assert_eq!(m.parent_sid, 0, "not under the caller's pane");
    assert_eq!(m.cwd, "/w", "it starts where the caller is");
    assert_eq!(out.get("id").and_then(|v| v.i64()), Some(77));
    assert_eq!(out.get("window").and_then(|v| v.i64()), Some(9));
    let recent = hub.recent.lock().unwrap();
    let parent = recent.opened_after(c.obs.at, Instant::now()).find(|o| o.sid == 77).map(|o| o.parent);
    assert_eq!(parent, Some(0), "listed with no parent");
}

#[test]
fn pane_control_open_in_a_new_window_waits_for_a_core_that_does_it() {
    let (hub, rx) = hub_at(&temp("newwin-old"));
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w")], Config::default());
    let mut conn = Conn::new(&hub, Peer::default(), None);
    let r = ops_pane::open(&mut conn, &c, &new_window()).unwrap_err();
    assert!(matches!(r.code, ErrorCode::OldSession | ErrorCode::CoreRestarting), "{r:?}");
    assert!(rx.try_recv().is_err(), "a core that does not do it is not asked");
}
