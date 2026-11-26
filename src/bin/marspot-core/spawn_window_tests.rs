//! A pane asked for in a new window, against real `marspot-session`
//! processes in the boot-assembly tests' sandbox.

use super::super::tests::{app_with_parent, drive, frame, reaped, sandbox, wait_for};
use super::*;
use marspot::shell_proto::{
    PaneRetire, PaneSpawned, RetireMode, decode_pane_spawned, decode_window_close_request,
    decode_window_open_request, encode_pane_retire, encode_pane_spawn,
};
use marspot_term::session_registry as reg;

fn ask(req_id: u32, parent_sid: u64) -> Frame {
    let m = PaneSpawn {
        req_id,
        shadow: false,
        no_focus: true,
        helper: false,
        new_window: true,
        parent_sid,
        idle_s: 0,
        cols: 0,
        rows: 0,
        cwd: String::new(),
    };
    frame(MsgType::PaneSpawn, encode_pane_spawn(&m).unwrap())
}

fn spawned(app: &CoreApp, req_id: u32) -> Option<PaneSpawned> {
    let all = app
        .pane_control
        .reports
        .frames()
        .filter(|f| f.msg_type == MsgType::PaneSpawned);
    all.map(|f| decode_pane_spawned(&f.payload).unwrap())
        .find(|s| s.req_id == req_id)
}

fn to_shell(app: &CoreApp, t: MsgType) -> Vec<u32> {
    let of = app.pending_to_shell.iter().filter(|(m, _)| *m == t);
    match t {
        MsgType::WindowOpenRequest => of
            .map(|(_, p)| decode_window_open_request(p).unwrap())
            .collect(),
        _ => of
            .map(|(_, p)| decode_window_close_request(p).unwrap())
            .collect(),
    }
}

#[test]
fn a_pane_asked_for_in_a_new_window_is_alone_there_and_its_close_closes_the_window() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    assert_eq!(app.route_pane_control(ask(31, psid)), Route::Forwarded);
    assert_eq!(
        to_shell(&app, MsgType::WindowOpenRequest),
        [WINDOW_OPEN_USER],
        "L1 is asked for a window"
    );
    assert_eq!(
        app.windows.len(),
        1,
        "nothing opens before L1's window comes"
    );

    app.adopt_window(9, 800.0, 600.0, 2.0, None);
    let mut got = None;
    wait_for("the new window's pane was answered for", || {
        drive(&mut app, &rx);
        got = spawned(&app, 31);
        got.is_some()
    });
    let s = got.unwrap();
    assert!(s.ok, "the spawn failed: {}", s.err);
    assert_eq!(s.window_id, 9, "the answer names the new window");
    assert_eq!(app.windows.len(), 2);
    assert_eq!(
        app.windows[0].panes.len(),
        1,
        "the asking pane's window did not grow"
    );
    let (wi, _) = app
        .find_window_pane_by_sid(s.sid)
        .expect("the pane is in a window");
    assert_eq!(app.windows[wi].window_id, 9);
    assert_eq!(app.windows[wi].panes.len(), 1, "it is the only pane there");
    assert!(app.shadow_index(s.sid).is_none(), "not a shadow");
    assert_eq!(
        reg::read_session_entry(s.sid).unwrap().shadow_of,
        0,
        "no parent"
    );

    let pid = reg::read_session_entry(s.sid).unwrap().pid;
    let retire = PaneRetire {
        req_id: 32,
        sid: s.sid,
        mode: RetireMode::Recycle,
        shadow_only: false,
    };
    assert_eq!(
        app.route_pane_control(frame(MsgType::PaneRetire, encode_pane_retire(&retire))),
        Route::Answered(DoneCode::Ok)
    );
    assert_eq!(
        to_shell(&app, MsgType::WindowCloseRequest),
        [9],
        "the emptied window asks L1 to close it"
    );
    wait_for("the session's process ended", || reaped(pid));
}

#[test]
fn a_new_window_request_whose_window_comes_too_late_is_answered_and_dropped() {
    let sb = sandbox();
    let (mut app, _rx, psid) = app_with_parent(&sb);
    assert_eq!(app.route_pane_control(ask(41, psid)), Route::Forwarded);
    let later = Instant::now() + WINDOW_WITHIN + Duration::from_secs(1);
    assert!(app.take_new_window_spawn(later).is_none());
    let s = spawned(&app, 41).expect("answered");
    assert!(!s.ok && s.sid == 0);
    assert!(
        app.take_new_window_spawn(Instant::now()).is_none(),
        "nothing left for the next window"
    );
}
