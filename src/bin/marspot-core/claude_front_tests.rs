//! A real shell on a terminal of its own, with a program whose argv[0]
//! is `claude` in front of it, for the /model guard's process check.

use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use marspot::pidtree::{claude_in_front, proc_row};

pub(crate) struct ClaudeFront {
    shell: Child,
    master: std::fs::File,
    /// The job's process group while it is in front.
    job: Option<i32>,
}

impl ClaudeFront {
    /// The shell's pid, which the core knows a pane's shell by.
    pub(crate) fn shell_pid(&self) -> i32 {
        self.shell.id() as i32
    }

    /// Start a shell in `dir` and a `claude` in front of it.
    pub(crate) fn start(dir: &Path) -> ClaudeFront {
        let claude = dir.join("claude");
        if !claude.exists() {
            std::os::unix::fs::symlink("/bin/sleep", &claude).unwrap();
        }
        let (master, slave) = pty();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-i").env_clear().env("PS1", "$ ").current_dir("/");
        cmd.stdin(Stdio::from(slave.try_clone().unwrap())).stdout(Stdio::from(slave.try_clone().unwrap())).stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe; fd 0 is the slave
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let shell = cmd.spawn().unwrap();
        let master = std::fs::File::from(master);
        drain(&master);
        let mut front = ClaudeFront { shell, master, job: None };
        let pid = front.shell_pid();
        assert!(wait_for(|| proc_row(pid).is_some_and(|r| r.tty_fg_pgid == r.pgid)), "the shell never took its terminal");
        writeln!(front.master, "{} 60", claude.display()).unwrap();
        assert!(wait_for(|| claude_in_front(pid)), "claude never came to the front");
        front.job = proc_row(pid).map(|r| r.tty_fg_pgid);
        front
    }

    /// Claude exits; the shell is back in front with its prompt.
    pub(crate) fn exit(&mut self) {
        let pid = self.shell_pid();
        if let Some(job) = self.job.take() {
            // SAFETY: a signal to the process group this test started
            unsafe { libc::kill(-job, libc::SIGKILL) };
        }
        assert!(wait_for(|| proc_row(pid).is_some_and(|r| r.tty_fg_pgid == r.pgid)), "the shell never came back");
        assert!(!claude_in_front(pid));
    }
}

impl Drop for ClaudeFront {
    fn drop(&mut self) {
        if let Some(job) = self.job {
            // SAFETY: a signal to the process group this test started
            unsafe { libc::kill(-job, libc::SIGKILL) };
        }
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

fn pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills the two descriptors; no name, settings or size asked for
    let r = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(r, 0);
    // SAFETY: both descriptors were just opened and are owned here
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Keeps the master drained so the shell never blocks on a full terminal.
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
