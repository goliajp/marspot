//! The two-level rule on every path that changes whose child a pane is,
//! against a core the test plays: `pane.parent`, a move into the
//! shadows, and a new pane asked for under a child pane.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use marspot::shell_proto::{
    CAP_SHADOWS, DoneCode, MsgType, PaneDone, PaneSpawned, PlaceWhere, decode_pane_place, decode_pane_spawn, encode_pane_done,
    encode_pane_spawned,
};
use marspot_peer::control::{ErrorCode, OpenArgs, Where};
use marspot_term::session_registry::SessionEntry;

use super::super::caller::{CallerKey, Peer};
use super::super::config::Config;
use super::super::conn::Conn;
use super::super::ops_parent::{self, HAS_CHILDREN, OWN_PARENT, PARENT_IS_CHILD};
use super::super::{Hub, MainCmd, ops_pane};
use super::{ctx, entry, hub_at, temp};

/// 1 and 2 are top panes, 3 is 1's child.
fn family() -> Vec<SessionEntry> {
    let child = |id, parent| SessionEntry { shadow_of: parent, ..entry(id, "/w") };
    vec![entry(1, "/w"), entry(2, "/w"), child(3, 1)]
}

/// What the core was asked: `(message, sid, to or spawn, parent)`.
type Asked = Mutex<Vec<(MsgType, u64, Option<PlaceWhere>, u64)>>;

/// Run `body` while a core answers every frame queued, Ok.
fn with_core<R>(hub: &Hub, rx: Receiver<MainCmd>, asked: &Asked, body: impl FnOnce() -> R) -> R {
    hub.caps.store(CAP_SHADOWS, Ordering::Release);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let stop = &stop;
        s.spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let Ok(MainCmd::Send { msg, payload, .. }) = rx.recv_timeout(Duration::from_millis(20)) else { continue };
                if let Some(Ok(m)) = (msg == MsgType::PanePlace).then(|| decode_pane_place(&payload)) {
                    asked.lock().unwrap().push((msg, m.sid, Some(m.to), m.host_sid));
                    let done = PaneDone { req_id: m.req_id, code: DoneCode::Ok, text: String::new() };
                    hub.links.route(MsgType::PaneDone, &encode_pane_done(&done).unwrap());
                } else if let Some(Ok(m)) = (msg == MsgType::PaneSpawn).then(|| decode_pane_spawn(&payload)) {
                    asked.lock().unwrap().push((msg, 0, None, m.parent_sid));
                    let s = PaneSpawned { req_id: m.req_id, ok: true, sid: 900, err: String::new(), window_id: 0 };
                    hub.links.route(MsgType::PaneSpawned, &encode_pane_spawned(&s).unwrap());
                }
            }
        });
        let out = body();
        stop.store(true, Ordering::Release);
        out
    })
}

#[test]
fn pane_control_parent_refuses_a_third_level_on_every_path() {
    let (hub, rx) = hub_at(&temp("parent"));
    let asked: Asked = Mutex::new(Vec::new());
    let user = ctx(CallerKey::External, family(), Config::default());
    let from_child = ctx(CallerKey::Pane(3), family(), Config::default());
    let from_top = ctx(CallerKey::Pane(2), family(), Config::default());
    let refused = |r: Result<_, marspot_peer::control::Reject>, why: &str| {
        let r = r.expect_err(why);
        assert_eq!((r.code, r.message.as_str()), (ErrorCode::BadRequest, why));
    };
    with_core(&hub, rx, &asked, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        refused(ops_parent::set_sid(&mut conn, &user, 2, 3), PARENT_IS_CHILD);
        refused(ops_parent::set_sid(&mut conn, &user, 1, 2), HAS_CHILDREN);
        refused(ops_parent::set_sid(&mut conn, &user, 2, 2), OWN_PARENT);
        // a move into the shadows makes the caller the parent
        refused(ops_pane::place_sid(&mut conn, &from_child, 2, Where::Shadow, false), PARENT_IS_CHILD);
        refused(ops_pane::place_sid(&mut conn, &from_top, 1, Where::Shadow, false), HAS_CHILDREN);
        assert!(asked.lock().unwrap().is_empty(), "nothing refused reached the core");

        // the sidebar's menu goes through the same check
        let menu = |sid, parent| marspot::shell_proto::HelperPanelAct {
            sid,
            act: marspot::shell_proto::PanelAct::SetParent,
            want: marspot::shell_proto::CwdControl::Flip,
            parent,
        };
        let deny = temp("parent-deny").join("control-deny.tsv");
        assert_eq!(super::super::panel_act::run(&mut conn, &user, menu(2, 3), &deny), (0, 1));
        assert!(asked.lock().unwrap().is_empty(), "nothing refused reached the core");

        ops_parent::set_sid(&mut conn, &user, 2, 1).expect("a leaf under a top pane");
        ops_parent::set_sid(&mut conn, &user, 3, 0).expect("a child made a top pane");
    });
    assert_eq!(
        *asked.lock().unwrap(),
        [(MsgType::PanePlace, 2, Some(PlaceWhere::Parent), 1), (MsgType::PanePlace, 3, Some(PlaceWhere::Parent), 0)]
    );
}

/// A new pane asked for under a child pane, by the child itself or by
/// naming it, opens under the child's top pane.
#[test]
fn pane_control_parent_of_a_new_pane_under_a_child_is_its_top() {
    let (hub, rx) = hub_at(&temp("parent-open"));
    let asked: Asked = Mutex::new(Vec::new());
    let from_child = ctx(CallerKey::Pane(3), family(), Config::default());
    let open = OpenArgs { cwd: Some("/w".into()), shadow: true, new_window: false, parent: None, cols: 80, rows: 24, command: None };
    with_core(&hub, rx, &asked, || {
        let mut conn = Conn::new(&hub, Peer::default(), None);
        ops_pane::open(&mut conn, &from_child, &open).expect("opened");
    });
    assert_eq!(*asked.lock().unwrap(), [(MsgType::PaneSpawn, 0, None, 1)], "under the top pane, not the child");
}
