//! Keeping what a child should not inherit from crossing its exec.
//!
//! A descriptor without `FD_CLOEXEC` crosses `exec`. Rust opens its own
//! with the flag, but some come from elsewhere: the frame region L2 hands
//! to L3 on purpose, the Metal shader caches a framework opens in L2, a
//! pipe a test runner created in another thread between `pipe()` and
//! setting the flag (macOS has no `pipe2`). Without this, a pane's shell
//! and every program it ran held a writable mapping of the pane's frame,
//! a unix socket and L2's Metal caches.

use std::os::fd::RawFd;

/// The descriptors worth closing run from 3 to this. Read before fork:
/// `getrlimit` is not on the list of calls a forked child may make in a
/// multithreaded parent.
pub fn close_limit() -> RawFd {
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    let cur = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } == 0 {
        r.rlim_cur
    } else {
        0
    };
    // an unlimited soft limit is not a loop bound
    cur.clamp(256, 1 << 16) as RawFd
}

/// Mark every descriptor from 3 below `limit`, except those in `keep`,
/// to close when the child execs.
///
/// Marked rather than closed: until the exec happens the child may
/// still need one of them -- `std::process::Command` reports a failed
/// exec back to the parent through a pipe of its own, and closing that
/// pipe would turn a failed spawn into one that looks successful. That
/// pipe already carries the flag, so marking leaves it working, and a
/// successful exec closes everything marked.
///
/// # Safety
/// For the child between fork and exec: it only calls `fcntl`, which is
/// async-signal-safe.
pub unsafe fn close_inherited(limit: RawFd, keep: &[RawFd]) {
    for fd in 3..limit {
        if keep.contains(&fd) {
            continue;
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::process::Command;
    use std::os::unix::process::CommandExt;

    /// Which of `fds` a child process finds open after exec.
    fn open_in_child(fds: &[RawFd], keep: &'static [RawFd]) -> String {
        let list = fds.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" ");
        let limit = close_limit();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("for f in {list}; do [ -e /dev/fd/$f ] && printf 'open%s ' $f; done; true"));
        unsafe {
            cmd.pre_exec(move || {
                close_inherited(limit, keep);
                Ok(())
            });
        }
        String::from_utf8(cmd.output().unwrap().stdout).unwrap()
    }

    /// A descriptor opened without the flag, the way an inherited one
    /// arrives.
    fn leaky_fd() -> std::fs::File {
        let f = std::fs::File::open("/dev/null").unwrap();
        let fd = f.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
        f
    }

    #[test]
    fn a_descriptor_without_the_flag_does_not_reach_the_child() {
        let a = leaky_fd();
        let b = leaky_fd();
        let fds = [a.as_raw_fd(), b.as_raw_fd()];
        // control: without the call both arrive
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("[ -e /dev/fd/{} ] && printf open", fds[0]));
        assert_eq!(String::from_utf8(cmd.output().unwrap().stdout).unwrap(), "open");
        assert_eq!(open_in_child(&fds, &[]), "");
    }

    #[test]
    fn a_kept_descriptor_still_reaches_the_child() {
        let a = leaky_fd();
        let fd = a.as_raw_fd();
        let keep: &'static [RawFd] = Box::leak(vec![fd].into_boxed_slice());
        assert_eq!(open_in_child(&[fd], keep), format!("open{fd} "));
    }

    #[test]
    fn a_failed_exec_is_still_reported() {
        let limit = close_limit();
        let mut cmd = Command::new("/nonexistent/marspot-session");
        unsafe {
            cmd.pre_exec(move || {
                close_inherited(limit, &[]);
                Ok(())
            });
        }
        assert!(cmd.spawn().is_err(), "the spawn must fail, not look like it worked");
    }
}
