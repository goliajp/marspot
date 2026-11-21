//! The directory a pane is working in, as a person reading its title
//! would mean it.

use std::path::PathBuf;

use super::{proc_cwd, proc_row};

/// The directory of the program in the foreground of `shell_pid`'s
/// terminal, or the shell's own when the shell is in front.
///
/// A pane running an agent is working where the agent was started,
/// whatever the shell under it last `cd`'d to: an agent that was
/// launched with `( cd dir && exec claude )` leaves the shell where it
/// was.  The foreground group's id is its leader's pid; a leader that
/// has exited leaves nothing to read, and the shell's directory is the
/// best answer left.
pub fn pane_cwd(shell_pid: i32) -> Option<PathBuf> {
    let shell = proc_row(shell_pid)?;
    let fg = shell.tty_fg_pgid;
    if fg > 0
        && fg != shell.pgid
        && let Some(cwd) = proc_cwd(fg)
    {
        return Some(cwd);
    }
    proc_cwd(shell_pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// An interactive shell on a terminal of its own, the way a pane
    /// runs one; only an interactive shell hands the terminal to a job.
    fn shell_on_a_pty() -> (std::process::Child, std::fs::File) {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills the two descriptors; no name, settings or size asked for.
        let r = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(r, 0);
        // SAFETY: both descriptors were just opened and are owned here.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-i"])
            .env_clear()
            .env("PS1", "$ ")
            .current_dir("/")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe; fd 0 is the slave.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        (child, std::fs::File::from(master))
    }

    /// Keeps the master side drained so the shell never blocks on a
    /// full terminal while the test looks at it.
    fn drain(master: &std::fs::File) {
        let mut m = master.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(std::io::Read::read(&mut m, &mut buf), Ok(n) if n > 0) {}
        });
    }

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_job_that_cd_d_in_a_subshell_names_the_pane_not_the_shell() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let (mut sh, mut master) = shell_on_a_pty();
        drain(&master);
        let pid = sh.id() as i32;
        let line = format!("(cd '{}' && exec sleep 5)\n", dir.display());
        std::io::Write::write_all(&mut master, line.as_bytes()).unwrap();
        let in_front = wait_for(|| proc_row(pid).is_some_and(|r| r.tty_fg_pgid > 0 && r.tty_fg_pgid != r.pgid));
        // the subshell is in front from its fork, before its `cd` has run
        let _ = wait_for(|| pane_cwd(pid).as_deref() == Some(dir.as_path()));
        let seen = pane_cwd(pid);
        let shell_dir = proc_cwd(pid);
        let _ = sh.kill();
        let _ = sh.wait();
        assert!(in_front, "the job never took the foreground");
        assert_eq!(shell_dir.as_deref(), Some(std::path::Path::new("/")));
        assert_eq!(seen.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn a_shell_in_front_names_the_pane_by_its_own_directory() {
        let (mut sh, master) = shell_on_a_pty();
        drain(&master);
        let pid = sh.id() as i32;
        let ready = wait_for(|| proc_row(pid).is_some_and(|r| r.tty_fg_pgid == r.pgid));
        let seen = pane_cwd(pid);
        let _ = sh.kill();
        let _ = sh.wait();
        assert!(ready);
        assert_eq!(seen.as_deref(), Some(std::path::Path::new("/")));
    }

    #[test]
    fn a_gone_process_has_no_directory() {
        assert_eq!(pane_cwd(i32::MAX), None);
    }
}
