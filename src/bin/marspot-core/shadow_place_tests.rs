//! Moving panes between a window and the shadows, against real
//! `marspot-session` processes.

use super::super::shadows::tests::{app_with_parent, done_for, drive, frame, open_shadow, sandbox, wait_done, wait_for};
use super::*;
use crate::window_state_tests::{app_in, win};
use marspot::shell_proto::{decode_pane_done, encode_pane_place};
use marspot_term::session_registry as reg;

fn place(req_id: u32, sid: u64, to: PlaceWhere, host_sid: u64) -> Frame {
    frame(MsgType::PanePlace, encode_pane_place(&PanePlace { req_id, sid, to, host_sid, force: false }))
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
fn shadow_place_of_two_panes_at_once_keeps_one_in_the_window() {
    let sb = sandbox();
    let (tx, rx) = mpsc::channel();
    let a = reg::allocate_next_session_id().unwrap();
    let b = reg::allocate_next_session_id().unwrap();
    let pa = spawn_l3_pane_with_cwd(60, 16, a, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    let pb = spawn_l3_pane_with_cwd(60, 16, b, "", &tx).expect("real L3 spawn");
    let mut app = app_in(sb.dir(), vec![win(1, vec![pa, pb])]);
    app.event_tx = tx;
    wait_for("both sessions read pane control", || {
        drive(&mut app, &rx);
        [a, b].iter().all(|&s| {
            app.pane_by_sid_mut(s)
                .is_some_and(|p| p.session().pane_control_reach() == marspot::pane::ControlReach::Reads)
        })
    });

    // each is asked to become the other's shadow before either session answers
    assert_eq!(app.route_pane_control(place(30, a, PlaceWhere::Shadow, b)), Route::Forwarded);
    assert_eq!(app.route_pane_control(place(31, b, PlaceWhere::Shadow, a)), Route::Forwarded);
    let (da, db) = (wait_done(&mut app, &rx, 30), wait_done(&mut app, &rx, 31));
    let (moved, kept, refused) = if da.code == DoneCode::Ok { (a, b, &db) } else { (b, a, &da) };
    assert_eq!(app.shadow_index(moved), Some(0), "one of them moved: {da:?} {db:?}");
    assert_eq!((refused.code, refused.text.as_str()), (DoneCode::Other, LAST_PANE_STAYS));
    assert!(app.find_window_pane_by_sid(kept).is_some(), "the last live pane stays in its window");
    assert_eq!(app.windows.len(), 1);
    wait_for("the pane that stayed is no shadow in its session", || {
        drive(&mut app, &rx);
        reg::read_session_entry(kept).is_ok_and(|e| e.shadow_of == 0)
    });
    assert_eq!(reg::read_session_entry(moved).unwrap().shadow_of, kept);
}

#[test]
fn shadow_place_under_an_unknown_parent_is_refused() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = open_shadow(&mut app, &rx, 7, psid);
    assert_eq!(app.route_pane_control(place(40, sid, PlaceWhere::Window, psid)), Route::Forwarded);
    assert_eq!(wait_done(&mut app, &rx, 40).code, DoneCode::Ok);
    app.route_pane_control(place(41, sid, PlaceWhere::Shadow, 0x7777_0001));
    let d = done_for(&app, 41).expect("answered at once");
    assert_eq!((d.code, d.text.as_str()), (DoneCode::Other, "no such parent pane"));
    assert!(app.find_window_pane_by_sid(sid).is_some());
}
