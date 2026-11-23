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
    let (tx, rx) = channel();
    (SessionListener::from_handoff(SID, inherited(), 4242, 80, 24, 0, tx).unwrap(), rx)
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
