//! `pane.list` right after `pane.open` and `pane.close`, from an
//! observation taken before either.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use marspot::shell_proto::{
    CAP_SHADOWS, DoneCode, MsgType, PaneDone, PaneSpawned, decode_pane_retire, decode_pane_size, decode_pane_spawn, encode_pane_done,
    encode_pane_spawned,
};
use marspot_term::session_registry::{SessionEntry, write_session_entry};
use marspot_peer::control::{ErrorCode, OpenArgs};
use marspot_peer::json::Value;

use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::Conn;
use super::super::{Hub, MainCmd, ops_pane, ops_read};
use super::{ctx, entry, hub_at, temp};

/// Run `body` while a core answers every frame `answer` takes.
fn with_core<R>(hub: &Hub, rx: Receiver<MainCmd>, answer: impl Fn(&Hub, MsgType, &[u8]) + Send + Sync, body: impl FnOnce() -> R) -> R {
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let (stop, answer) = (&stop, &answer);
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

fn spawn_as(sid: u64) -> impl Fn(&Hub, MsgType, &[u8]) + Send + Sync {
    move |hub, msg, p| {
        let Some(Ok(m)) = (msg == MsgType::PaneSpawn).then(|| decode_pane_spawn(p)) else { return };
        let s = PaneSpawned { req_id: m.req_id, ok: true, sid, err: String::new() };
        hub.links.route(MsgType::PaneSpawned, &encode_pane_spawned(&s).unwrap());
    }
}

fn row(list: &Value, sid: u64) -> Option<&Value> {
    list.get("panes")?.arr().iter().find(|p| p.get("id").and_then(Value::i64) == Some(sid as i64))
}

#[test]
fn pane_control_open_answers_the_name_the_next_list_shows() {
    let (hub, rx) = hub_at(&temp("openname"));
    // the view is from before the open, as the observer's can be
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w/spg")], Config::default());
    let open = OpenArgs { cwd: None, shadow: true, parent: None, cols: 120, rows: 40, command: None };
    let (opened, list) = with_core(&hub, rx, spawn_as(4242), || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        let opened = ops_pane::open(&mut conn, &c, &open).unwrap();
        (opened, ops_read::list(&conn, &c))
    });
    let name = opened.get("name").and_then(Value::str).unwrap().to_string();
    assert!(name.starts_with("spg"), "the open answered {name:?}");
    let row = row(&list, 4242).unwrap_or_else(|| panic!("the new pane is not listed: {list:?}"));
    assert_eq!(row.get("name").and_then(Value::str), Some(name.as_str()));
    assert_eq!(row.get("parent").and_then(Value::i64), Some(10));
    assert_eq!(row.get("shadow").and_then(Value::bool), Some(true));
}

/// A pane the registry has, its process this test's own, so it counts
/// as alive.
fn registered(sid: u64, parent: u64) -> SessionEntry {
    let e = SessionEntry { pid: std::process::id() as i32, shadow_of: parent, ..entry(sid, "/w/spg") };
    write_session_entry(&e).unwrap();
    e
}

#[test]
fn pane_control_list_right_after_a_close_leaves_the_pane_out() {
    let _sandbox = marspot_term::test_env::sandbox_state_dir();
    let (hub, rx) = hub_at(&temp("closelist"));
    // the view still has the pane, as one taken before the close or
    // while its session is ending does
    let panes = vec![entry(10, "/w/spg"), registered(4242, 10), registered(4243, 10)];
    let c = ctx(CallerKey::Pane(10), panes, Config::default());
    let retired = |hub: &Hub, msg: MsgType, p: &[u8]| {
        let Some(Ok(m)) = (msg == MsgType::PaneRetire).then(|| decode_pane_retire(p)) else { return };
        let done = PaneDone { req_id: m.req_id, code: DoneCode::Ok, text: String::new() };
        hub.links.route(MsgType::PaneDone, &encode_pane_done(&done).unwrap());
    };
    let (before, closed, after) = with_core(&hub, rx, retired, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        let before = ops_read::list(&conn, &c);
        let closed = ops_pane::close(&mut conn, &c, "4242");
        (before, closed, ops_read::list(&conn, &c))
    });
    assert!(row(&before, 4242).is_some(), "the view lists it before the close: {before:?}");
    closed.unwrap();
    assert!(row(&after, 4242).is_none(), "listed after its close: {after:?}");
    assert!(row(&after, 4243).is_some(), "another pane went too: {after:?}");
}

#[test]
fn pane_control_resize_right_after_an_open_reaches_the_core() {
    let _sandbox = marspot_term::test_env::sandbox_state_dir();
    let (hub, rx) = hub_at(&temp("openresize"));
    // the registry has both; the view, from before the open, only the window pane
    registered(10, 0);
    registered(4244, 10);
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w/spg")], Config::default());
    let open = OpenArgs { cwd: None, shadow: true, parent: None, cols: 120, rows: 40, command: None };
    let answer = |hub: &Hub, msg: MsgType, p: &[u8]| {
        spawn_as(4244)(hub, msg, p);
        let Some(Ok(m)) = (msg == MsgType::PaneSize).then(|| decode_pane_size(p)) else { return };
        let done = PaneDone { req_id: m.req_id, code: DoneCode::Ok, text: String::new() };
        hub.links.route(MsgType::PaneDone, &encode_pane_done(&done).unwrap());
    };
    let (window, resized) = with_core(&hub, rx, answer, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        ops_pane::open(&mut conn, &c, &open).unwrap();
        (ops_pane::resize(&mut conn, &c, "10", 100, 30), ops_pane::resize(&mut conn, &c, "4244", 100, 30))
    });
    assert_eq!(window.map(|_| ()).map_err(|r| r.code), Err(ErrorCode::NotResizable));
    resized.unwrap_or_else(|r| panic!("the view is older than the pane, and the resize was refused: {r:?}"));
}

#[test]
fn pane_control_only_a_helpers_pane_is_said_to_be_one() {
    let (hub, rx) = hub_at(&temp("openhelper"));
    let c = ctx(CallerKey::Pane(10), vec![entry(10, "/w/spg")], Config::default());
    let open = OpenArgs { cwd: None, shadow: true, parent: None, cols: 0, rows: 0, command: None };
    let said = std::sync::Mutex::new(Vec::new());
    let answer = |hub: &Hub, msg: MsgType, p: &[u8]| {
        if let Some(Ok(m)) = (msg == MsgType::PaneSpawn).then(|| decode_pane_spawn(p)) {
            said.lock().unwrap().push(m.helper);
        }
        spawn_as(4245)(hub, msg, p);
    };
    with_core(&hub, rx, answer, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        ops_pane::open(&mut conn, &c, &open).unwrap();
        ops_pane::open_helper(&mut conn, &c, &open).unwrap();
    });
    assert_eq!(*said.lock().unwrap(), [false, true]);
}
