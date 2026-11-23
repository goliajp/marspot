//! Shadow panes, the session's half: a session started as another's
//! shadow says so in its first registry entry, ends when its parent's
//! process does or when it sits idle, stops being one when the core
//! says so, and stays what it is across an image swap.

use std::path::Path;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{ClusterTableReader, SessionRegions, open_region};
use marspot_term::session_registry::{read_session_entry, session_dir, session_socket_path};
use marspot_term::shell_proto::{DoneCode, MsgType, SessionShadow, encode_session_shadow};

use crate::pane_link::{Link, Reply, bin, screen_shows, wait};
use crate::self_execv::{Sandbox, count_in_log, stage};
use crate::support::{COLS, ROWS, Session};

fn sid(n: u64) -> u64 {
    crate::support::id_base() + 4300 + n
}

/// Session `sid(n)` running `body` (a `/bin/sh` script that prints
/// READY), the shadow of `shadow.0` with timeout `shadow.1` if given,
/// with its registry entry written.
fn pane(sandbox: &mut Sandbox, n: u64, body: &str, shadow: Option<(u64, u32)>) -> (Session, SessionRegions) {
    use std::os::unix::fs::PermissionsExt;
    let script = sandbox.path().join(format!("pane{n}.sh"));
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.env.set("SHELL", &script);
    match shadow {
        Some((parent, idle_s)) => {
            sandbox.env.set("MARSPOT_SHADOW_OF", parent.to_string());
            sandbox.env.set("MARSPOT_SHADOW_IDLE_S", idle_s.to_string());
        }
        None => {
            sandbox.env.remove("MARSPOT_SHADOW_OF");
            sandbox.env.remove("MARSPOT_SHADOW_IDLE_S");
        }
    }
    let (s, regions) = Session::spawn_named(bin(), COLS, ROWS, sid(n));
    sandbox.env.remove("MARSPOT_SHADOW_OF");
    sandbox.env.remove("MARSPOT_SHADOW_IDLE_S");
    let pid = s.pid() as i32;
    wait(10.0, "the registry entry", || read_session_entry(sid(n)).is_ok_and(|e| e.pid == pid));
    screen_shows(&s, "READY");
    (s, regions)
}

fn parent(sandbox: &mut Sandbox, n: u64) -> (Session, SessionRegions) {
    pane(sandbox, n, "printf READY; exec sleep 600", None)
}

fn shadow_of(n: u64) -> u64 {
    read_session_entry(sid(n)).expect("the entry").shadow_of
}

/// Kill without collecting, as a parent whose own parent has not
/// collected it yet: the shadow must not take the zombie for alive.
fn kill(s: &Session) {
    // SAFETY: a process this test started
    unsafe { libc::kill(s.pid() as i32, libc::SIGKILL) };
}

/// How long `s` took to exit, if it did within `secs`.
fn exits_within(s: &mut Session, secs: f64) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs_f64(secs) {
        if s.child.try_wait().unwrap().is_some() {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn assert_gone(n: u64, regions: &SessionRegions) {
    assert!(!session_dir(sid(n)).exists(), "session {n}'s directory is gone");
    assert!(open_region(regions.grid_name()).is_err(), "session {n}'s grid region name is gone");
    assert!(ClusterTableReader::open(regions).is_err(), "session {n}'s cluster region name is gone");
}

fn set_shadow(s: &Session, req_id: u32, parent_sid: u64) -> (DoneCode, String) {
    let mut link = Link::new(s);
    s.send_frame(MsgType::SessionShadow, encode_session_shadow(&SessionShadow { req_id, parent_sid, idle_s: 0 }));
    link.take(5.0, &format!("PaneDone {req_id}"), |r| match r {
        Reply::Done(d) if d.req_id == req_id => Some((d.code, d.text.clone())),
        _ => None,
    })
}

/// Ask for an image swap and reattach to the image that took over.
fn swap(s: &Session, state: &Path, n: u64) -> Link {
    stage(state, true);
    let before = count_in_log(state, "L3_EXECV_RESUMED");
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    let pid = s.pid() as i32;
    wait(30.0, "the new image", || {
        count_in_log(state, "L3_EXECV_RESUMED") > before && unsafe { libc::kill(pid, 0) } == 0
    });
    Link::on(crate::support::reattach(&session_socket_path(sid(n)), 15).expect("reattach after the swap"))
}

#[test]
fn shadow_session_first_entry_names_the_parent_and_ends_with_it() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf \"READY [$MARSPOT_SHADOW_OF]\"; exec sleep 600", Some((sid(1), 0)));
    assert_eq!(shadow_of(2), sid(1), "the entry written at bind names the parent");
    screen_shows(&b, "READY []");
    assert!(ClusterTableReader::open(&rb).is_ok(), "the shadow's regions are there before");
    kill(&a);
    let took = exits_within(&mut b, 2.0).expect("the shadow exits within 2 s of its parent");
    assert_gone(2, &rb);
    assert!(count_in_log(sandbox.path(), "L3_SHADOW_ENDED") >= 1, "{took:?}");
    let _ = a.child.wait();
    ra.delete();
}

/// A parent that cannot be confirmed at start: the shadow ends at once
/// rather than live on with nothing to end with.
#[test]
fn shadow_session_whose_parent_is_not_there_ends_at_once() {
    let mut sandbox = Sandbox::new();
    sandbox.env.set("SHELL", "/bin/sh");
    sandbox.env.set("MARSPOT_SHADOW_OF", sid(9).to_string());
    sandbox.env.remove("MARSPOT_SHADOW_IDLE_S");
    let (mut s, regions) = Session::spawn_named(bin(), COLS, ROWS, sid(4));
    sandbox.env.remove("MARSPOT_SHADOW_OF");
    exits_within(&mut s, 3.0).expect("a shadow whose parent is not there ends at once");
    assert_gone(4, &regions);
    assert!(count_in_log(sandbox.path(), "L3_SHADOW_ENDED") >= 1);
}

#[test]
fn shadow_session_nested_two_levels_ends_layer_by_layer() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf READY; exec sleep 600", Some((sid(1), 0)));
    let (mut c, rc) = pane(&mut sandbox, 3, "printf READY; exec sleep 600", Some((sid(2), 0)));
    assert_eq!(shadow_of(3), sid(2), "each names its direct parent");
    assert!(exits_within(&mut c, 0.5).is_none(), "the inner shadow lives while its parent does");
    kill(&a);
    exits_within(&mut b, 2.0).expect("the first level ends with the top");
    exits_within(&mut c, 2.0).expect("the second level ends with the first");
    assert_gone(2, &rb);
    assert_gone(3, &rc);
    let _ = a.child.wait();
    ra.delete();
}

#[test]
fn shadow_session_cleared_by_the_core_outlives_its_old_parent() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf READY; exec sleep 600", Some((sid(1), 0)));
    assert_eq!(set_shadow(&b, 41, sid(9)), (DoneCode::NoSuchSession, "no such session".into()));
    assert_eq!(shadow_of(2), sid(1), "a refused parent changes nothing");
    assert_eq!(set_shadow(&b, 42, 0).0, DoneCode::Ok);
    assert_eq!(shadow_of(2), 0, "the entry says it is no shadow when the answer comes");
    kill(&a);
    let _ = a.child.wait();
    assert!(exits_within(&mut b, 3.0).is_none(), "a pane that is no shadow outlives the old parent");
    b.kill();
    ra.delete();
    rb.delete();
}

#[test]
fn shadow_session_set_by_the_core_ends_with_its_new_parent() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf READY; exec sleep 600", None);
    assert_eq!(shadow_of(2), 0);
    assert_eq!(set_shadow(&b, 7, sid(1)).0, DoneCode::Ok);
    assert_eq!(shadow_of(2), sid(1));
    kill(&a);
    exits_within(&mut b, 2.0).expect("the shadow exits within 2 s of its new parent");
    assert_gone(2, &rb);
    let _ = a.child.wait();
    ra.delete();
}

#[test]
fn shadow_session_idle_timeout_ends_it_and_input_postpones_it() {
    let mut sandbox = Sandbox::new();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let quiet = "stty -echo; printf READY; exec sleep 600";
    let (mut b, rb) = pane(&mut sandbox, 2, quiet, Some((sid(1), 2)));
    let took = exits_within(&mut b, 6.0).expect("an idle shadow ends after its timeout");
    assert!(took >= Duration::from_millis(1500), "not before the timeout: {took:?}");
    assert_gone(2, &rb);

    let (mut c, rc) = pane(&mut sandbox, 3, quiet, Some((sid(1), 2)));
    let mut link = Link::new(&c);
    let typing = Instant::now();
    while typing.elapsed() < Duration::from_secs(5) {
        link.inject("x");
        assert!(exits_within(&mut c, 0.7).is_none(), "input postpones the timeout");
    }
    let took = exits_within(&mut c, 6.0).expect("ends once the input stops");
    assert!(took >= Duration::from_millis(1000), "counted from the last input: {took:?}");
    assert_gone(3, &rc);
    a.kill();
    ra.delete();
}

#[test]
fn shadow_session_survives_an_image_swap_and_its_position_does_not_go_back() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf READY; exec sleep 600", Some((sid(1), 0)));
    let before = Link::new(&b).pos();
    assert!(before > 0, "READY was counted");
    let mut after = swap(&b, &state, 2);
    assert!(after.pos() >= before, "the position does not go back across the swap");
    assert_eq!(shadow_of(2), sid(1), "still a shadow after the swap");
    kill(&a);
    exits_within(&mut b, 2.0).expect("the new image watches the parent too");
    assert_gone(2, &rb);
    let _ = a.child.wait();
    ra.delete();
}

#[test]
fn shadow_session_moved_to_a_window_stays_normal_across_a_swap() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let (mut a, ra) = parent(&mut sandbox, 1);
    let (mut b, rb) = pane(&mut sandbox, 2, "printf READY; exec sleep 600", Some((sid(1), 0)));
    assert_eq!(set_shadow(&b, 5, 0).0, DoneCode::Ok);
    let _link = swap(&b, &state, 2);
    assert_eq!(shadow_of(2), 0, "the new image writes no parent");
    kill(&a);
    let _ = a.child.wait();
    assert!(exits_within(&mut b, 3.0).is_none(), "nor follows the old one");
    b.kill();
    ra.delete();
    rb.delete();
}
