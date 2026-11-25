// these change the process-wide inherited list: one process per test (nextest)
use std::process::Command;
use std::time::{Duration, Instant};

use super::{adopt, disown, reap_pending};

/// Whether child `pid` has ended, without waiting for it; None when it
/// is not a child of this process (any more).
fn ended(pid: i32) -> Option<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0 {
        return None;
    }
    Some(info.si_pid == pid)
}

#[test]
fn inherited_disown_tells_whether_a_child_was_already_waited_for() {
    let exited = Command::new("/usr/bin/true").spawn().unwrap().id() as i32;
    let running = Command::new("/bin/sleep").arg("30").spawn().unwrap().id() as i32;
    let deadline = Instant::now() + Duration::from_secs(5);
    while ended(exited) != Some(true) {
        assert!(Instant::now() < deadline, "the first child did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    adopt();
    assert_eq!(ended(exited), None, "waited for at once");
    assert!(disown(exited), "already waited for: its new owner must leave it alone");
    assert!(!disown(exited), "told once");
    assert!(!disown(running), "still running: its new owner waits for it");
    assert_eq!(reap_pending(), 0, "a disowned child is not looked at again");
    unsafe { libc::kill(running, libc::SIGKILL) };
    let mut st = 0;
    assert_eq!(unsafe { libc::waitpid(running, &mut st, 0) }, running, "left for its new owner to wait for");
}

/// A child the shell starts before `adopt` would be taken for an
/// inherited one; one it starts after `adopt` is fine.  Read from the
/// source, since a test cannot run `main` itself.
#[test]
fn inherited_adopt_runs_in_main_before_anything_can_start_a_child() {
    let src = include_str!("main.rs");
    let main = &src[src.find("\nfn main()").expect("fn main")..];
    let adopt = main.find("inherited::adopt()").expect("main adopts the inherited children");
    let settle = main.find("apple_events::settle_after_exec()").expect("main settles the registration");
    assert!(adopt < settle, "adopt before settle_after_exec");
    for spawn in ["Command::", ".spawn("] {
        assert!(main.find(spawn).is_none_or(|at| adopt < at), "adopt before the first {spawn}");
    }
}
