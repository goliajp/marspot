//! Who holds a pane's terminal: the shell, or a program it started.
//!
//! Bytes typed into a pane go to whichever process group the terminal
//! has in front, and nothing else.  A command line meant for the shell
//! is input to any other program: on 2026-11-17 a codex resume line was
//! typed into claude, after a wait for "the shell's new prompt" had
//! been satisfied by claude redrawing its own screen.  Output going
//! quiet tells nothing about who reads next; the terminal's foreground
//! group does.
//!
//! The foreground group is the whole test.  The terminal's settings do
//! not tell more: a shell's line editor turns off canonical input and
//! echo while it reads a line, and so does every full-screen program,
//! claude included.

use marspot::pidtree;

/// Who holds a shell's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Front {
    /// The shell itself: it is at its prompt, reading a line.
    Shell,
    /// A program the shell started, alive.
    Program { pid: i32, comm: String },
    /// Nobody that can be named: the job in front has just exited and
    /// the shell has not taken the terminal back yet, or the shell is
    /// gone.  Says why.
    Unknown(String),
}

impl Front {
    /// `Ok` when the shell holds its terminal, otherwise why not.
    pub fn shell_or_why(self) -> Result<(), String> {
        match self {
            Front::Shell => Ok(()),
            Front::Program { pid, comm } => Err(format!("pid {pid} ({comm}) holds the terminal, not the shell")),
            Front::Unknown(why) => Err(why),
        }
    }
}

/// Who holds `shell`'s terminal, asked of the kernel now.
pub fn of(shell: i32) -> Front {
    let Some(sh) = pidtree::proc_row(shell) else {
        return Front::Unknown(format!("the shell (pid {shell}) is gone"));
    };
    let fg = sh.tty_fg_pgid;
    if sh.tty_dev == 0 || fg <= 0 {
        return Front::Unknown("the terminal has no foreground group".into());
    }
    if fg == sh.pgid {
        return Front::Shell;
    }
    match pidtree::proc_row(fg) {
        Some(r) if r.tty_dev == sh.tty_dev && r.status != libc::SZOMB => Front::Program { pid: r.pid, comm: r.comm },
        _ => Front::Unknown(format!("group {fg} holds the terminal and its leader has exited")),
    }
}

/// The program in front of `shell`'s terminal, when it is one of the
/// shell's jobs and `pred` takes it.
pub fn program_in_front(shell: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<pidtree::ProcId> {
    let Front::Program { pid, .. } = of(shell) else {
        return None;
    };
    let row = pidtree::proc_row(pid)?;
    // a job is a descendant of the shell; a pid the shell's group id
    // happens to name elsewhere is not
    let mut up = row.ppid;
    for _ in 0..64 {
        if up == shell {
            return pred(&row).then(|| row.id());
        }
        if up <= 1 {
            return None;
        }
        up = pidtree::proc_row(up)?.ppid;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// An interactive shell on a terminal of its own, the way a pane
    /// runs one.
    pub(crate) fn shell_on_a_pty() -> (std::process::Child, std::fs::File) {
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
        let m = std::fs::File::from(master);
        // kept drained, so the shell never blocks on a full terminal
        let mut reader = m.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
        });
        (child, m)
    }

    pub(crate) fn wait_for(mut done: impl FnMut() -> bool) -> bool {
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
    fn a_shell_at_its_prompt_holds_its_terminal() {
        let (mut sh, _m) = shell_on_a_pty();
        let pid = sh.id() as i32;
        let seen = wait_for(|| of(pid) == Front::Shell);
        let _ = sh.kill();
        let _ = sh.wait();
        assert!(seen);
    }

    #[test]
    fn a_job_in_front_is_named_and_found_only_by_what_it_is() {
        let (mut sh, mut m) = shell_on_a_pty();
        let pid = sh.id() as i32;
        std::io::Write::write_all(&mut m, b"/bin/sleep 30\n").unwrap();
        let named = wait_for(|| matches!(of(pid), Front::Program { ref comm, .. } if comm == "sleep"));
        let is_sleep: fn(&pidtree::ProcRow) -> bool = |r| r.comm == "sleep";
        let is_cat: fn(&pidtree::ProcRow) -> bool = |r| r.comm == "cat";
        let found = program_in_front(pid, is_sleep);
        let other = program_in_front(pid, is_cat);
        let _ = sh.kill();
        let _ = sh.wait();
        assert!(named, "the sleep never took the terminal");
        assert!(found.is_some());
        assert!(other.is_none());
    }

    #[test]
    fn a_gone_shell_holds_nothing() {
        assert!(matches!(of(i32::MAX), Front::Unknown(_)));
    }
}
