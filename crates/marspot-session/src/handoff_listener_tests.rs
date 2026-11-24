use super::*;
use marspot_term::session_registry::{mark_closed, read_session_entry};
use marspot_term::test_env::sandbox_state_dir;
use std::os::fd::IntoRawFd;
use std::sync::mpsc::{Receiver, channel};

const SID: u64 = 31;

fn inherited() -> RawFd {
    // not in the sandbox, whose path can be longer than a socket's may be
    let path = format!("/tmp/msh-{}", std::process::id());
    let _ = std::fs::remove_file(&path);
    let fd = UnixListener::bind(&path).unwrap().into_raw_fd();
    std::fs::remove_file(&path).unwrap();
    fd
}

fn adopt() -> (SessionListener, Receiver<crate::SessionEvent>) {
    adopt_bound(None)
}

fn adopt_bound(socket: Option<crate::own_socket::SocketId>) -> (SessionListener, Receiver<crate::SessionEvent>) {
    let (tx, rx) = channel();
    (SessionListener::from_handoff(SID, inherited(), 4242, 80, 24, 0, socket, tx).unwrap(), rx)
}

#[test]
fn an_image_that_takes_over_writes_the_entry() {
    let _sandbox = sandbox_state_dir();
    std::fs::create_dir_all(session_dir(SID)).unwrap();
    let (_l, rx) = adopt();
    assert_eq!(read_session_entry(SID).unwrap().shell_child_pid, 4242);
    assert!(rx.try_recv().is_err());
}

#[test]
fn an_image_that_takes_over_a_closed_session_closes_it() {
    let _sandbox = sandbox_state_dir();
    std::fs::create_dir_all(session_dir(SID)).unwrap();
    mark_closed(SID).unwrap();
    let (_l, rx) = adopt();
    assert!(matches!(rx.try_recv(), Ok(crate::SessionEvent::CloseRequested)));
    assert!(read_session_entry(SID).is_err(), "the entry was not written again");
}

#[test]
fn an_image_that_takes_over_does_not_make_a_dir_that_went() {
    let _sandbox = sandbox_state_dir();
    let (_l, _rx) = adopt();
    assert!(!session_dir(SID).exists());
}

/// The directory was taken over before the swap: the socket at the path
/// is the new owner's, and the image that takes over leaves it there.
#[test]
fn an_image_that_takes_over_removes_only_the_socket_the_first_one_bound() {
    use crate::own_socket::SocketId;
    let _sandbox = sandbox_state_dir();
    std::fs::create_dir_all(session_dir(SID)).unwrap();
    let ours = session_dir(SID).join("ours");
    std::fs::write(&ours, "").unwrap();
    let theirs = session_socket_path(SID);
    std::fs::write(&theirs, "").unwrap();
    adopt_bound(SocketId::of(&ours)).0.leave_unreachable();
    assert!(theirs.exists(), "the new owner's socket is left");
    adopt_bound(SocketId::of(&theirs)).0.leave_unreachable();
    assert!(!theirs.exists(), "its own goes");
}

/// An image too old to carry the socket's id handed over: the socket at
/// the path is the one it bound, and it goes with the session.
#[test]
fn an_image_that_takes_over_from_one_without_the_socket_id_removes_the_socket() {
    let _sandbox = sandbox_state_dir();
    std::fs::create_dir_all(session_dir(SID)).unwrap();
    let left = session_socket_path(SID);
    std::fs::write(&left, "").unwrap();
    adopt().0.leave_unreachable();
    assert!(!left.exists(), "the socket the old image bound is left behind");
}
