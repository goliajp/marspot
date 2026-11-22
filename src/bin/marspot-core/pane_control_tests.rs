use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use marspot::shell_proto::{
    PaneType, PaneWatch, WatchBody, WatchKind, decode_pane_control_caps, decode_pane_done,
    encode_pane_type, encode_pane_watch,
};
use marspot_term::session_registry as reg;

fn typing(sid: u64, req_id: u32) -> Frame {
    let m = PaneType {
        req_id,
        sid,
        paste: false,
        enter: false,
        only_at_shell: false,
        bytes: Vec::new(),
        watch: None,
    };
    Frame::new(MsgType::PaneType, encode_pane_type(&m).unwrap())
}

fn watch(sid: u64, req_id: u32) -> Frame {
    let body = WatchBody {
        watch_id: 5,
        kind: WatchKind::Quiet,
        since_pos: 0,
        quiet_ms: 100,
        text: String::new(),
    };
    Frame::new(
        MsgType::PaneWatch,
        encode_pane_watch(&PaneWatch {
            req_id,
            sid,
            watch: body,
        })
        .unwrap(),
    )
}

/// `(req_id, code)` of every `PaneDone` held for L1.
fn answers(app: &CoreApp) -> Vec<(u32, DoneCode)> {
    app.pane_control
        .reports
        .frames()
        .filter(|f| f.msg_type == MsgType::PaneDone)
        .map(|f| decode_pane_done(&f.payload).unwrap())
        .map(|d| (d.req_id, d.code))
        .collect()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn l2_watch_and_typing_for_an_unknown_session_answer_not_attached() {
    let sb = Sandbox::new();
    let vacant = reg::allocate_next_session_id().unwrap();
    let mut app = app_in(
        sb.dir(),
        vec![win(1, vec![Pane::new_vacant(vacant, 60, 16)])],
    );
    assert_eq!(
        app.route_pane_control(watch(0xDEAD_BEEF, 1)),
        Route::Answered(DoneCode::NotAttached)
    );
    // a placeholder slot has no session connection either
    assert_eq!(
        app.route_pane_control(typing(vacant, 2)),
        Route::Answered(DoneCode::NotAttached)
    );
    assert_eq!(
        answers(&app),
        [(1, DoneCode::NotAttached), (2, DoneCode::NotAttached)]
    );
}

#[test]
fn l2_watch_too_short_for_a_request_id_is_not_answered() {
    let sb = Sandbox::new();
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant()])]);
    let route = app.route_pane_control(Frame::new(MsgType::PaneWatch, vec![1, 2]));
    assert_eq!(route, Route::Unanswerable);
    assert!(app.pane_control.reports.is_empty());
    // long enough for its ids but not its body: answered, not forwarded
    let mut short = typing(9, 3).payload;
    short.truncate(13);
    let route = app.route_pane_control(Frame::new(MsgType::PaneType, short));
    assert_eq!(route, Route::Answered(DoneCode::Other));
}

#[test]
fn l2_typing_and_watch_caps_go_to_l1_once_after_hello() {
    let mut pc = PaneControl::default();
    pc.caps_after_hello();
    pc.caps_after_hello();
    let caps: Vec<_> = pc.reports.frames().collect();
    assert_eq!(caps.len(), 1, "once");
    assert_eq!(caps[0].msg_type, MsgType::PaneControlCaps);
    assert_eq!(
        decode_pane_control_caps(&caps[0].payload).unwrap(),
        CAP_TYPE_AND_WATCH
    );
}

#[test]
fn l2_forwards_typing_and_watch_once_the_session_says_it_reads_them() {
    let mut sb = Sandbox::new();
    let shell = sb.dir().join("shell");
    std::fs::write(&shell, "#!/bin/sh\nIFS= read -r a\nexit 0\n").unwrap();
    std::fs::set_permissions(
        &shell,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    sb.set("SHELL", &shell.to_string_lossy());
    let (tx, _rx) = mpsc::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx)
        .expect("real L3 spawn (is marspot-session built?)");
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant()])]);
    app.shadows.push(pane);
    let says = |app: &mut CoreApp, ty: MsgType| {
        app.pane_by_sid_mut(sid)
            .unwrap()
            .session_mut()
            .l3_session_says(&Frame::new(ty, Vec::new()));
    };

    assert_eq!(
        app.route_pane_control(typing(sid, 1)),
        Route::Answered(DoneCode::OldSession)
    );
    assert_eq!(
        app.route_pane_control(watch(sid, 2)),
        Route::Answered(DoneCode::OldSession)
    );

    says(&mut app, MsgType::SessionReadsPaneControl);
    assert_eq!(app.route_pane_control(typing(sid, 3)), Route::Forwarded);
    assert_eq!(app.route_pane_control(watch(sid, 4)), Route::Forwarded);

    app.reconnecting.insert(sid);
    assert_eq!(
        app.route_pane_control(watch(sid, 5)),
        Route::Answered(DoneCode::NotAttached)
    );
    app.reconnecting.remove(&sid);

    // a new image publishes, and until it says so it is taken for an old one
    says(&mut app, MsgType::SessionPublishes);
    assert_eq!(
        app.route_pane_control(typing(sid, 6)),
        Route::Answered(DoneCode::OldSession)
    );
    assert_eq!(
        answers(&app),
        [
            (1, DoneCode::OldSession),
            (2, DoneCode::OldSession),
            (5, DoneCode::NotAttached),
            (6, DoneCode::OldSession)
        ],
        "the forwarded ones are the session's to answer"
    );

    app.inject_input(sid, b"\r");
    wait_for("the session's shell ended and its pane closed", || {
        if let Some(p) = app.shadows.first_mut() {
            p.pump();
        }
        app.close_ended_panes();
        app.shadows.is_empty()
    });
}

#[test]
fn l2_forwards_watch_reports_from_a_session_to_l1() {
    let sb = Sandbox::new();
    let mut app = app_in(sb.dir(), vec![win(1, vec![vacant()])]);
    let fired = Frame::new(MsgType::PaneWatchFired, vec![0; 31]);
    let typed = Frame::new(MsgType::PaneTyped, vec![1; 26]);
    app.report_to_shell(fired.clone());
    app.report_to_shell(typed.clone());
    let held: Vec<_> = app
        .pane_control
        .reports
        .frames()
        .map(|f| (f.msg_type, f.payload.clone()))
        .collect();
    assert_eq!(
        held,
        [
            (fired.msg_type, fired.payload),
            (typed.msg_type, typed.payload)
        ],
        "unchanged, in order"
    );
}

fn vacant() -> Pane {
    Pane::new_vacant(reg::allocate_next_session_id().unwrap(), 60, 16)
}
