//! Shadow panes in the core, against real `marspot-session` processes
//! in the boot-assembly tests' sandbox.

use super::super::shadow_place::UNANSWERED;
use super::super::spawn::spawn_l3_shadow_with_cwd;
use super::*;
use crate::boot_assembly_tests::Sandbox;
use crate::window_state_tests::{app_in, win};
use marspot::shell_proto::{
    PaneDone, PanePlace, PaneRetire, PaneSize, PlaceWhere, decode_pane_done, decode_pane_spawned,
    encode_pane_place, encode_pane_retire, encode_pane_size,
};
use marspot::state::{SavedPane, SavedWindowLayout};
use marspot_term::session_registry as reg;
use std::sync::mpsc::Receiver;

/// Every session started here runs this shell: it reads lines until
/// one says `q`, then ends.
fn sandbox() -> Sandbox {
    let mut sb = Sandbox::new();
    let shell = sb.dir().join("shell");
    std::fs::write(&shell, "#!/bin/sh\nwhile IFS= read -r a; do [ \"$a\" = q ] && exit 0; done\n").unwrap();
    std::fs::set_permissions(&shell, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755))
        .unwrap();
    sb.set("SHELL", &shell.to_string_lossy());
    sb
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What the loop does with the events these tests care about.
fn drive(app: &mut CoreApp, rx: &Receiver<CoreEvent>) {
    while let Ok(ev) = rx.try_recv() {
        match ev {
            CoreEvent::L3SpawnFinished(sid, o) => app.adopt_spawn_outcome(sid, o),
            CoreEvent::L3Says(sid, f) => {
                if let Some(p) = app.pane_by_sid_mut(sid) {
                    p.session_mut().l3_session_says(&f);
                }
            }
            CoreEvent::PaneReport(sid, f) => app.report_to_shell(sid, f),
            _ => {}
        }
    }
}

fn done_for(app: &CoreApp, req_id: u32) -> Option<PaneDone> {
    app.pane_control
        .reports
        .frames()
        .filter(|f| f.msg_type == MsgType::PaneDone)
        .map(|f| decode_pane_done(&f.payload).unwrap())
        .find(|d| d.req_id == req_id)
}

fn wait_done(app: &mut CoreApp, rx: &Receiver<CoreEvent>, req_id: u32) -> PaneDone {
    let mut got = None;
    wait_for(&format!("an answer to request {req_id}"), || {
        drive(app, rx);
        got = done_for(app, req_id);
        got.is_some()
    });
    got.unwrap()
}

/// A window with one live pane, and the channel its sessions report on.
fn app_with_parent(sb: &Sandbox) -> (CoreApp, Receiver<CoreEvent>, u64) {
    let (tx, rx) = mpsc::channel();
    let psid = reg::allocate_next_session_id().unwrap();
    let parent = spawn_l3_pane_with_cwd(60, 16, psid, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    let mut app = app_in(sb.dir(), vec![win(1, vec![parent])]);
    app.event_tx = tx;
    (app, rx, psid)
}

/// Open a shadow pane under `parent` and wait until it is answered and
/// its session reads pane control.
fn open_shadow(app: &mut CoreApp, rx: &Receiver<CoreEvent>, req_id: u32, parent: u64) -> u64 {
    assert_eq!(app.dev_open_shadow(req_id, parent), Route::Forwarded);
    let mut spawned = None;
    wait_for("the shadow pane's spawn was answered", || {
        drive(app, rx);
        spawned = app
            .pane_control
            .reports
            .frames()
            .filter(|f| f.msg_type == MsgType::PaneSpawned)
            .map(|f| decode_pane_spawned(&f.payload).unwrap())
            .find(|s| s.req_id == req_id);
        spawned.is_some()
    });
    let s = spawned.unwrap();
    assert!(s.ok, "the spawn failed: {}", s.err);
    wait_for("the shadow's session says it reads pane control", || {
        drive(app, rx);
        app.pane_by_sid_mut(s.sid)
            .is_some_and(|p| p.session().pane_control_reach() == marspot::pane::ControlReach::Reads)
    });
    s.sid
}

fn frame(t: MsgType, p: Vec<u8>) -> Frame {
    Frame::new(t, p)
}

#[test]
fn shadow_open_resize_and_close_through_the_dev_hook() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = open_shadow(&mut app, &rx, 7, psid);

    assert_eq!(app.shadow_index(sid), Some(0), "in the shadows");
    assert!(app.find_window_pane_by_sid(sid).is_none(), "in no window");
    assert_eq!(app.windows[0].panes.len(), 1, "the window did not grow");
    assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, psid, "the entry names its parent");
    assert!(unwatched::is(sid), "its frames wake nobody");

    let size = |req_id, sid| frame(MsgType::PaneSize, encode_pane_size(&PaneSize { req_id, sid, cols: 100, rows: 30 }));
    assert_eq!(app.route_pane_control(size(8, sid)), Route::Answered(DoneCode::Ok));
    assert_eq!(app.route_pane_control(size(9, psid)), Route::Answered(DoneCode::Other));
    assert_eq!(done_for(&app, 9).unwrap().text, "not_resizable");

    // a parent nobody knows opens nothing
    let before = app.shadows.len();
    assert_eq!(app.dev_open_shadow(10, 0xDEAD_BEEF), Route::Answered(DoneCode::Other));
    assert_eq!(app.shadows.len(), before);

    let pid = reg::read_session_entry(sid).unwrap().pid;
    let retire = frame(MsgType::PaneRetire, encode_pane_retire(&PaneRetire { req_id: 11, sid, mode: RetireMode::Delete }));
    assert_eq!(app.route_pane_control(retire), Route::Answered(DoneCode::Ok));
    assert!(app.shadows.is_empty());
    assert!(!unwatched::is(sid));
    // the dir itself is not asserted: a session still writing while
    // the close removes its dir can leave files behind, on the close
    // path every pane shares
    wait_for("the session's process ended", || {
        // SAFETY: the session is this test process's child; this reaps it
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) == pid }
    });
    let again = frame(MsgType::PaneRetire, encode_pane_retire(&PaneRetire { req_id: 12, sid, mode: RetireMode::Delete }));
    assert_eq!(app.route_pane_control(again), Route::Answered(DoneCode::NoSuchSession));
}

#[test]
fn shadow_sessions_go_back_to_the_shadows_at_boot_and_a_saved_window_wins() {
    let sb = sandbox();
    let (tx, rx) = mpsc::channel();
    let psid = reg::allocate_next_session_id().unwrap();
    let parent = spawn_l3_pane_with_cwd(60, 16, psid, "", &tx).expect("real L3 spawn");
    let env = ShadowEnv { parent: psid, idle_s: 0 };
    let asid = reg::allocate_next_session_id().unwrap();
    let a = spawn_l3_shadow_with_cwd(60, 16, asid, env, &tx).expect("shadow L3 spawn");
    let bsid = reg::allocate_next_session_id().unwrap();
    let b = spawn_l3_shadow_with_cwd(60, 16, bsid, env, &tx).expect("shadow L3 spawn");
    for sid in [asid, bsid] {
        assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, psid, "the first entry is a shadow's");
    }
    // the core that started them goes; the sessions stay
    std::mem::forget((parent, a, b));
    while rx.try_recv().is_ok() {}

    let saved = SavedWindowLayout {
        grid_cols: 2,
        grid_rows: 1,
        focused_idx: 0,
        panes: [psid, bsid]
            .iter()
            .map(|&sid| SavedPane { sid, flags: 0, custom_title: String::new(), last_cwd: String::new() })
            .collect(),
    };
    let (panes, _) = assemble_panes_at_boot(Some(&saved), 2, 60, 16, &tx, true, &Default::default());
    let window_sids: Vec<_> = panes.iter().filter_map(Pane::shelld_session_id).collect();
    assert_eq!(window_sids, [psid, bsid], "the saved window holds its panes, the shadow among them");
    let taken = window_sids.iter().copied().collect();
    let shadows = shadow_boot::adopt_shadows_at_boot(&taken, &tx);
    let shadow_sids: Vec<_> = shadows.iter().filter_map(Pane::shelld_session_id).collect();
    assert_eq!(shadow_sids, [asid], "the other shadow is back in the shadows, not in the window");
    assert!(unwatched::is(asid) && !unwatched::is(bsid));

    let mut app = app_in(sb.dir(), vec![win(1, panes)]);
    app.shadows = shadows;
    wait_for("the session in the window is a shadow no longer", || {
        drive(&mut app, &rx);
        reg::read_session_entry(bsid).is_ok_and(|e| e.shadow_of == 0)
    });
    std::thread::sleep(Duration::from_millis(200));
    drive(&mut app, &rx);
    assert!(
        app.pane_control.reports.frames().all(|f| f.msg_type != MsgType::PaneDone),
        "the session's answer to the core's own request did not go on to L1"
    );
    assert_eq!(reg::read_session_entry(asid).unwrap().shadow_of, psid, "the other is still a shadow");
}

fn place(req_id: u32, sid: u64, to: PlaceWhere, host_sid: u64) -> Frame {
    frame(MsgType::PanePlace, encode_pane_place(&PanePlace { req_id, sid, to, host_sid }))
}

#[test]
fn shadow_place_moves_after_the_answer_and_without_one_puts_the_pane_back() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = open_shadow(&mut app, &rx, 7, psid);

    assert_eq!(app.route_pane_control(place(20, sid, PlaceWhere::Window, psid)), Route::Forwarded);
    assert!(app.shadow_index(sid).is_some(), "nothing moves before the session answers");
    assert_eq!(wait_done(&mut app, &rx, 20).code, DoneCode::Ok);
    assert_eq!(app.find_window_pane_by_sid(sid), Some((0, 1)), "into the parent's window");
    assert!(app.shadows.is_empty() && !unwatched::is(sid));
    assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, 0);

    assert_eq!(app.route_pane_control(place(21, sid, PlaceWhere::Shadow, psid)), Route::Forwarded);
    assert_eq!(wait_done(&mut app, &rx, 21).code, DoneCode::Ok);
    assert_eq!(app.shadow_index(sid), Some(0));
    assert!(unwatched::is(sid));
    assert_eq!(reg::read_session_entry(sid).unwrap().shadow_of, psid);

    // a session that cannot answer
    let pid = reg::read_session_entry(sid).unwrap().pid;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    assert_eq!(app.route_pane_control(place(22, sid, PlaceWhere::Window, psid)), Route::Forwarded);
    app.expire_places(Instant::now() + Duration::from_secs(4));
    let d = done_for(&app, 22).expect("answered at the deadline");
    assert_eq!((d.code, d.text.as_str()), (DoneCode::Other, UNANSWERED));
    assert_eq!(app.shadow_index(sid), Some(0), "the pane stayed in the shadows");
    assert_eq!(app.windows[0].panes.len(), 2, "the window holds the parent and the placeholder only");

    // its late answer is undone
    assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
    wait_for("the late answer arrived", || {
        drive(&mut app, &rx);
        app.pane_control.shadows.places.expired.is_empty()
    });
    wait_for("the session is the parent's shadow again", || {
        drive(&mut app, &rx);
        reg::read_session_entry(sid).is_ok_and(|e| e.shadow_of == psid)
    });
    assert!(app.pane_control.reports.frames().all(|f| decode_pane_done(&f.payload).map_or(true, |d| d.req_id < 0xF000_0000)));
}

#[test]
fn shadow_whose_shell_exits_leaves_the_shadows() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = open_shadow(&mut app, &rx, 7, psid);
    app.inject_input(sid, b"q\r");
    wait_for("the shadow left the shadows", || {
        drive(&mut app, &rx);
        app.pump_all();
        app.shadows.is_empty()
    });
    assert!(!app.every_session_lost(), "the parent is alive");
    assert!(!unwatched::is(sid));
}

#[test]
fn shadow_panes_count_toward_every_session_lost() {
    let sb = Sandbox::new();
    let mut app = app_in(sb.dir(), vec![win(1, vec![Pane::new_vacant(reg::allocate_next_session_id().unwrap(), 60, 16)])]);
    assert!(app.every_session_lost(), "a vacant pane is a lost session");
    app.shadows.push(Pane::new_pending(reg::allocate_next_session_id().unwrap(), 60, 16));
    assert!(!app.every_session_lost(), "a shadow still starting is not lost");
}

#[test]
fn shadow_clipboard_writes_bells_and_frames_are_dropped() {
    use std::os::unix::net::UnixStream;
    let mut sb = Sandbox::new();
    let _pb = crate::real_pty_tests::OwnPasteboard::new(&mut sb);
    let token = format!("marspot-shadow-clip-{}", std::process::id());
    let run = |sid: u64| {
        let (mut session, core) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let (selection_tx, _selection_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || l3_reader_loop(core, sid, tx, selection_tx));
        for (t, p) in [
            (MsgType::GridReady, Vec::new()),
            (MsgType::PaneBell, Vec::new()),
            (MsgType::PaneClipboardWrite, token.as_bytes().to_vec()),
            (MsgType::PaneAwake, Vec::new()),
        ] {
            Frame::new(t, p).write_to(&mut session).unwrap();
        }
        drop(session);
        reader.join().unwrap();
        rx.try_iter().collect::<Vec<CoreEvent>>()
    };

    unwatched::set(91, true);
    let events = run(91);
    assert!(matches!(&events[0], CoreEvent::L3Says(91, f) if f.msg_type == MsgType::PaneAwake), "{events:?}");
    assert!(matches!(events[1], CoreEvent::L3ControlEof(91)));
    assert_eq!(events.len(), 2, "no frame wake and no bell");
    assert_eq!(marspot::input::read_clipboard_text(), None, "the clipboard was not written");

    // the same frames from a watched session do all three
    unwatched::set(91, false);
    let events = run(91);
    assert!(matches!(events[0], CoreEvent::L3Ready));
    assert!(matches!(&events[1], CoreEvent::PaneBell(91, _)));
    assert_eq!(marspot::input::read_clipboard_text().as_deref(), Some(token.as_str()));
}
