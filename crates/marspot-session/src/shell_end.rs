//! The pane's shell ends with the pane, on the path that exits.
//!
//! A session that is closed saves its state and calls `exit`, which
//! runs no destructor: the terminal's own teardown (hang up, wait,
//! kill) never happened there, and the shell was left to the hangup the
//! kernel sends when the terminal closes.  A shell acts on that hangup
//! in a signal handler, and one that takes it at the wrong moment does
//! not come out: zsh, expanding a prompt that runs a command, was found
//! with the parent in `fflush` and the child it had just forked waiting
//! for the same stdio lock, both for good, the pane gone.

use std::time::{Duration, Instant};

/// How long a shell gets to act on the hangup.
const GRACE: Duration = Duration::from_millis(100);

fn gone(pid: i32) -> bool {
    // SAFETY: collects our child if it has exited; signal 0 only asks
    // whether the pid still takes a signal.
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) > 0 || libc::kill(pid, 0) != 0 }
}

/// End the shell of session `sid` with its pane, and say so when the
/// hangup was not enough.
pub(crate) fn end_with_pane(sid: u64, shell: i32) {
    if end(shell) {
        marspot_term::lx_event!(
            "L3_SHELL_KILLED",
            "the pane's shell did not go on the hangup; killed with the pane",
            session_id = sid,
            shell_pid = shell
        );
    }
}

/// Hang the shell up, and kill it if it is still there after the
/// grace.  Returns whether it had to be killed.
fn end(shell: i32) -> bool {
    if shell <= 1 || gone(shell) {
        return false;
    }
    // SAFETY: signals to the pid of the shell this session started.
    unsafe {
        libc::kill(shell, libc::SIGCONT);
        libc::kill(shell, libc::SIGHUP);
    }
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if gone(shell) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: as above.
    unsafe { libc::kill(shell, libc::SIGKILL) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn a_shell_that_ignores_the_hangup_is_killed_and_one_that_obeys_is_not() {
        let mut deaf = Command::new("/bin/sh").args(["-c", "trap '' HUP; sleep 30"]).process_group(0).spawn().unwrap();
        // the trap has to be in place before the hangup is sent
        std::thread::sleep(Duration::from_millis(300));
        assert!(end(deaf.id() as i32), "the shell that ignores it had to be killed");
        assert!(deaf.wait().is_ok());
        // SAFETY: the group is the one this test made; its `sleep` outlives the shell.
        unsafe { libc::kill(-(deaf.id() as i32), libc::SIGKILL) };

        let mut obedient = Command::new("/bin/sleep").arg("30").process_group(0).spawn().unwrap();
        assert!(!end(obedient.id() as i32), "a hangup is enough for a process that does not ignore it");
        let _ = obedient.wait();
        assert!(!end(obedient.id() as i32), "nothing to do for one that is gone");
    }
}
