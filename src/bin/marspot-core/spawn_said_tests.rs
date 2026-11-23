use super::super::pane_control::Route;
use super::super::shadows::tests::{app_with_parent, drive, sandbox, wait_for};
use super::*;
use marspot::shell_proto::decode_pane_spawned;

fn answered(app: &CoreApp, req_id: u32) -> Option<u64> {
    let frames = app.pane_control.reports.frames().filter(|f| f.msg_type == MsgType::PaneSpawned);
    let mut said = frames.map(|f| decode_pane_spawned(&f.payload).unwrap()).filter(|s| s.req_id == req_id);
    said.next().map(|s| s.sid)
}

/// Open a shadow pane up to its spawn's landing; what its session said
/// meanwhile is kept from the core.
fn landed(app: &mut CoreApp, rx: &Receiver<CoreEvent>, req_id: u32, parent: u64) -> u64 {
    assert_eq!(app.dev_open_shadow(req_id, parent), Route::Forwarded);
    loop {
        match rx.recv_timeout(Duration::from_secs(30)).expect("the spawn lands") {
            CoreEvent::L3SpawnFinished(sid, o) => {
                app.adopt_spawn_outcome(sid, o);
                return sid;
            }
            _ => {}
        }
    }
}

#[test]
fn shadow_open_is_answered_when_the_session_reads_pane_control() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = landed(&mut app, &rx, 7, psid);
    assert_eq!(answered(&app, 7), None, "a request on the answer would be refused");
    assert!(app.pane_control.shadows.wait().is_some_and(|w| w <= SAY_WITHIN), "the loop wakes for it");
    wait_for("the answer", || {
        drive(&mut app, &rx);
        answered(&app, 7).is_some()
    });
    assert_eq!(answered(&app, 7), Some(sid));
    let reach = app.pane_by_sid_mut(sid).unwrap().session().pane_control_reach();
    assert_eq!(reach, marspot::pane::ControlReach::Reads);
    assert_eq!(app.pane_control.shadows.wait(), None, "nothing is left to wake for");
}

#[test]
fn shadow_open_of_a_session_that_never_says_it_is_answered_all_the_same() {
    let sb = sandbox();
    let (mut app, rx, psid) = app_with_parent(&sb);
    let sid = landed(&mut app, &rx, 8, psid);
    app.expire_places(Instant::now() + SAY_WITHIN * 2);
    assert_eq!(answered(&app, 8), Some(sid));
    assert_eq!(app.pane_control.shadows.wait(), None);
}
