//! An L3's update carries what its kind of exec lists and nothing else,
//! and the new image checks: the real binary, a real shell, real execs.
//! The test builds are debug builds, in which a new image that finds
//! something extra panics.

use std::os::fd::AsRawFd;

use marspot_term::shell_proto::MsgType;
use support::Session;

use crate::self_execv::{Sandbox, count_in_log, id, stage, start, wait_for};
use crate::support;

/// Ask for an update to a build other than the one running, and wait
/// for the new image's boot to end.
fn update(s: &mut Session, state: &std::path::Path, n: usize) {
    stage(state, n % 2 == 1);
    let before = count_in_log(state, "SESSION_OWNER");
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(20, &format!("update {n} to finish its boot"), || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited");
        (count_in_log(state, "SESSION_OWNER") > before).then_some(())
    });
}

/// Whether another process holds the session directory's lock.
fn dir_lock_held() -> bool {
    let path = marspot_term::session_registry::session_dir(id()).join(".lock");
    let f = std::fs::File::open(path).unwrap();
    // SAFETY: flock on a descriptor of ours
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    r != 0
}

#[test]
fn exec_inherits_only_registered_fds_and_env() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    assert!(dir_lock_held(), "the cold image holds the session directory");
    for n in 1..=3 {
        update(&mut s, &state, n);
        assert!(dir_lock_held(), "the lock crossed update {n}");
    }
    assert_eq!(count_in_log(&state, "L3_EXECV_RESUMED"), 3);
    assert_eq!(count_in_log(&state, "exec.contract.mismatch"), 0);
    s.kill();
}

/// The update hands over something extra, and the new image stops.
fn leaked(what: &str) {
    let mut sandbox = Sandbox::new();
    sandbox.env.set("MARSPOT_TEST_EXEC_LEAK", what);
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    stage(&state, true);
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    let status = wait_for(20, "the new image to refuse what it was handed", || s.child.try_wait().unwrap());
    assert!(!status.success(), "{status}");
    assert_eq!(count_in_log(&state, "L3_EXECV_INVOKE"), 1, "the old image did exec");
    assert_eq!(count_in_log(&state, "exec.contract.mismatch"), 1, "the new image found it");
    s.kill();
}

#[test]
fn exec_contract_an_extra_descriptor_stops_the_new_image() {
    leaked("fd");
}

#[test]
fn exec_contract_an_extra_variable_stops_the_new_image() {
    leaked("var");
}

/// The update had something extra on hand before its sweep: the new
/// image resumes and logs no mismatch.
fn resumes_cleanly(what: &str) {
    let mut sandbox = Sandbox::new();
    sandbox.env.set("MARSPOT_TEST_EXEC_LEAK", what);
    let mut s = start(&mut sandbox);
    let state = sandbox.path().to_path_buf();
    update(&mut s, &state, 1);
    assert_eq!(count_in_log(&state, "L3_EXECV_RESUMED"), 1);
    assert_eq!(count_in_log(&state, "exec.contract.mismatch"), 0);
    assert_eq!(s.child.try_wait().unwrap(), None, "the new image went on");
    s.kill();
}

#[test]
fn exec_contract_the_sweep_takes_a_descriptor_out_of_the_way() {
    resumes_cleanly("fd-before");
}

#[test]
fn exec_contract_the_sweep_takes_a_variable_out_of_the_way() {
    resumes_cleanly("var-before");
}
