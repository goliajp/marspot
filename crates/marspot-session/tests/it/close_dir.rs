//! A session that is closed removes its own directory, with no core to
//! do it; one that is killed, or whose directory is another session's
//! by now, removes nothing.

use marspot_term::session_registry::{mark_closed, read_session_entry, session_dir, write_session_entry};
use marspot_term::grid_shm::SessionRegions;

use crate::pane_link::{bin, screen_shows, wait};
use crate::self_execv::Sandbox;
use crate::support::{COLS, ROWS, Session};

fn sid(n: u64) -> u64 {
    crate::support::id_base() + 4700 + n
}

/// Session `sid(n)`, printing without end.
fn flooding(sandbox: &mut Sandbox, n: u64) -> (Session, SessionRegions) {
    use std::os::unix::fs::PermissionsExt;
    let script = sandbox.path().join("flood.sh");
    std::fs::write(&script, "#!/bin/sh\nexec /usr/bin/yes READY\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    sandbox.env.set("SHELL", &script);
    let (s, regions) = Session::spawn_named(bin(), COLS, ROWS, sid(n));
    let pid = s.pid() as i32;
    wait(10.0, "the registry entry", || read_session_entry(sid(n)).is_ok_and(|e| e.pid == pid));
    screen_shows(&s, "READY");
    (s, regions)
}

fn ends_on(s: &mut Session, sig: i32) {
    // SAFETY: a process this test started
    unsafe { libc::kill(s.pid() as i32, sig) };
    wait(20.0, "the session's exit", || s.child.try_wait().unwrap().is_some());
}

#[test]
fn close_removes_the_dir_of_a_session_in_full_output() {
    let mut sandbox = Sandbox::new();
    for n in 1..=5 {
        let (mut s, regions) = flooding(&mut sandbox, n);
        mark_closed(sid(n)).unwrap();
        ends_on(&mut s, libc::SIGHUP);
        assert!(!session_dir(sid(n)).exists(), "session {n}'s directory is gone when it exits");
        let left = std::fs::read_dir(sandbox.path().join("retired")).unwrap().count();
        assert_eq!(left, 0, "and is nowhere else");
        regions.delete();
    }
}

#[test]
fn close_of_a_killed_session_removes_nothing() {
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = flooding(&mut sandbox, 11);
    ends_on(&mut s, libc::SIGKILL);
    assert!(session_dir(sid(11)).join("entry.toml").exists(), "kept for the next core");
    regions.delete();
}

/// The entry names another process: the directory was taken over.
#[test]
fn close_leaves_a_dir_that_is_another_sessions_by_now() {
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = flooding(&mut sandbox, 12);
    let mut entry = read_session_entry(sid(12)).unwrap();
    entry.pid = 1;
    write_session_entry(&entry).unwrap();
    mark_closed(sid(12)).unwrap();
    ends_on(&mut s, libc::SIGHUP);
    assert_eq!(read_session_entry(sid(12)).unwrap(), entry, "left as the other wrote it");
    regions.delete();
}

/// A hangup from anyone ends the process and keeps the history.
#[test]
fn close_nobody_asked_for_keeps_the_dir_and_the_screen() {
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = flooding(&mut sandbox, 13);
    ends_on(&mut s, libc::SIGHUP);
    for file in ["entry.toml", "state.bin"] {
        assert!(session_dir(sid(13)).join(file).exists(), "{file}");
    }
    regions.delete();
}

/// The mark came too late for the main loop to act on it: the image
/// that hands over, or the one that takes over, does.
#[test]
fn close_of_a_session_that_is_updating_wins() {
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = flooding(&mut sandbox, 14);
    crate::self_execv::stage(sandbox.path(), true);
    mark_closed(sid(14)).unwrap();
    s.send_frame(marspot_term::shell_proto::MsgType::SessionUpdate, Vec::new());
    wait(30.0, "the session's exit", || s.child.try_wait().unwrap().is_some());
    assert!(!session_dir(sid(14)).exists());
    let log = std::fs::read_to_string(sandbox.path().join("logs/marspot.log")).unwrap();
    assert!(log.contains("\tL3_SHUTDOWN_EXECV\t"), "it was handing over");
    assert!(!log.contains("\tL3_EXECV_INVOKE\t"), "and did not");
    regions.delete();
}

/// The mark of a close that was cut short is not the next session's.
#[test]
fn close_mark_left_behind_is_not_the_next_sessions() {
    let mut sandbox = Sandbox::new();
    std::fs::create_dir_all(session_dir(sid(15))).unwrap();
    mark_closed(sid(15)).unwrap();
    let (mut s, regions) = flooding(&mut sandbox, 15);
    ends_on(&mut s, libc::SIGHUP);
    assert!(session_dir(sid(15)).join("state.bin").exists());
    regions.delete();
}

/// The directory was taken over, and its new session bound its socket
/// at the same path: the session that leaves removes nothing of it.
#[test]
fn close_by_a_new_owner_leaves_the_new_owners_socket() {
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = flooding(&mut sandbox, 16);
    let sock = session_dir(sid(16)).join("sock");
    std::fs::remove_file(&sock).unwrap();
    let _theirs = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let mut entry = read_session_entry(sid(16)).unwrap();
    entry.pid = 1;
    write_session_entry(&entry).unwrap();
    wait(20.0, "the session's exit", || s.child.try_wait().unwrap().is_some());
    assert!(sock.exists(), "the new owner's socket is there");
    regions.delete();
}
