use super::*;
use marspot_term::session_registry::{SessionEntry, session_dir, try_lock_session_dir, write_session_entry};
use marspot_term::test_env::{ChildGuard, sandbox_state_dir};

const SID: u64 = 7;

/// Not being able to look is not an exit: the L3 may be writing still.
#[test]
fn close_without_a_queue_to_wait_on_removes_nothing() {
    let _sandbox = sandbox_state_dir();
    let (_child, id) = l3();
    register(id.pid);
    clean_up_on(-1, SID, id, Duration::from_secs(20));
    assert!(session_dir(SID).exists());
}

/// A close that no core saw to its end: the next one removes what is
/// left, and nothing of a session that was not closed.
#[test]
fn close_is_finished_by_a_later_core() {
    let _sandbox = sandbox_state_dir();
    register(3_999_999);
    finish_closed();
    assert!(session_dir(SID).exists(), "not closed");
    mark_closed(SID).unwrap();
    let held = try_lock_session_dir(SID).unwrap();
    finish_closed();
    assert!(session_dir(SID).exists(), "held");
    drop(held);
    finish_closed();
    assert!(!session_dir(SID).exists());
}

/// A process that stands for the L3: `clean_up` looks at its exit and
/// at the entry, not at what it runs.
fn l3() -> (ChildGuard, ProcId) {
    let child = ChildGuard::new(std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let id = pidtree::proc_id(child.id() as i32).unwrap();
    (child, id)
}

fn register(pid: i32) {
    let socket = session_dir(SID).join("sock");
    write_session_entry(&SessionEntry {
        id: SID,
        pid,
        socket,
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: String::new(),
        proto_version: marspot_term::shell_proto::PROTO_VERSION,
        created_at_unix: 0,
        shm_name: String::new(),
        shell_child_pid: 0,
        build: String::new(),
        stamps_regions: false,
        shadow_of: 0,
    })
    .unwrap();
}

#[test]
fn close_waits_for_the_exit_of_an_l3_that_removes_nothing() {
    let _sandbox = sandbox_state_dir();
    let (mut child, id) = l3();
    register(id.pid);
    let cleaning = std::thread::spawn(move || clean_up(SID, id, Duration::from_secs(20)));
    std::thread::sleep(Duration::from_millis(300));
    assert!(session_dir(SID).exists(), "not while its L3 may write");
    child.kill().unwrap();
    cleaning.join().unwrap();
    assert!(!session_dir(SID).exists());
}

#[test]
fn close_of_a_stuck_l3_takes_its_dir_at_the_limit() {
    let _sandbox = sandbox_state_dir();
    let (mut child, id) = l3();
    register(id.pid);
    clean_up(SID, id, Duration::from_millis(200));
    assert!(!session_dir(SID).exists());
    assert!(child.try_wait().unwrap().is_none(), "it was not killed");
}

#[test]
fn close_leaves_the_dir_another_l3_registered_in() {
    let _sandbox = sandbox_state_dir();
    let (mut child, id) = l3();
    register(id.pid + 1);
    child.kill().unwrap();
    child.wait().unwrap();
    clean_up(SID, id, Duration::from_secs(20));
    assert!(session_dir(SID).exists());
}

#[test]
fn close_of_a_lost_session_leaves_a_dir_an_l3_holds() {
    let _sandbox = sandbox_state_dir();
    register(1);
    let held = try_lock_session_dir(SID).unwrap();
    close_dead_session(SID);
    assert!(session_dir(SID).exists(), "held");
    drop(held);
    close_dead_session(SID);
    assert!(!session_dir(SID).exists());
    let retired = marspot_term::paths::state_root().join("retired");
    let kept: Vec<_> = std::fs::read_dir(retired).unwrap().flatten().collect();
    assert!(kept.len() == 1 && kept[0].path().join("entry.toml").exists(), "its history is kept for a while");
    close_dead_session(SID);
    assert!(!session_dir(SID).exists(), "none is made to look");
}
