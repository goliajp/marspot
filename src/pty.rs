//! Pseudo-terminal management.
//!
//! Wraps macOS's `forkpty` syscall to spawn a child process with a controlling
//! terminal attached. Read/write happens on the master file descriptor; the
//! child's stdin/stdout/stderr are wired to the slave side.

use std::ffi::CString;
use std::io;
use std::os::raw::{c_char, c_int};
use std::os::unix::io::RawFd;
use std::ptr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PtyConfig {
    pub program: String,
    /// Extra args (does **not** include argv[0]; we push `program` for that).
    /// Matches `std::process::Command::args` semantics.
    pub args: Vec<String>,
    pub size: TerminalSize,
    /// Override for argv[0] passed to execvp.  `None` → use `program`
    /// directly.  `Some("-zsh")` → tells the shell to behave as a
    /// login shell (Unix convention: leading `-`).  iTerm2 / Terminal.app
    /// do this; marspot now matches so .zprofile / .bash_profile run and
    /// zsh's PROMPT_EOL_MARK doesn't fire on a fresh prompt because the
    /// non-login startup path leaves the cursor mid-line.
    pub argv0: Option<String>,
}

/// Owned handle to a spawned child process attached to a pseudo-terminal.
///
/// Drop sends SIGHUP, then SIGKILL after a short wait, and waits for the
/// child to be reaped. The master fd is closed in Drop. There is no path
/// where the fd or the child PID outlives the `Pty`.
pub struct Pty {
    master: RawFd,
    child: i32,
}

impl Pty {
    pub fn spawn(config: PtyConfig) -> io::Result<Self> {
        // Pre-allocate everything we'll need post-fork. The window between
        // fork and execv must only call async-signal-safe functions, so no
        // allocations after forkpty() returns in the child.
        let program = CString::new(config.program.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program path contains NUL"))?;
        let argv0 = match config.argv0.as_ref() {
            Some(s) => CString::new(s.as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv0 contains NUL"))?,
            None => program.clone(),
        };
        let arg_cstrings: Vec<CString> = config
            .args
            .iter()
            .map(|a| CString::new(a.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"))?;

        // argv[0] = override-or-program, argv[1..] = args, argv[last] = NULL
        let mut argv: Vec<*const c_char> = Vec::with_capacity(arg_cstrings.len() + 2);
        argv.push(argv0.as_ptr());
        for arg in &arg_cstrings {
            argv.push(arg.as_ptr());
        }
        argv.push(ptr::null());

        let winsize = libc::winsize {
            ws_row: config.size.rows,
            ws_col: config.size.cols,
            ws_xpixel: config.size.pixel_width,
            ws_ypixel: config.size.pixel_height,
        };

        let mut master_fd: c_int = -1;
        // SAFETY: forkpty is the standard POSIX/BSD primitive for spawning a
        // controlling-terminal child.  We pass NULL for the slave name (we
        // don't need it back) and NULL termios (use defaults).
        let pid = unsafe {
            libc::forkpty(
                &mut master_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                &winsize as *const _ as *mut _,
            )
        };

        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // Child.  Only async-signal-safe calls allowed here.
            unsafe {
                // execvp does PATH search for relative names (e.g. "tmux")
                // while still matching execv's behaviour for absolute
                // paths.  Strict execv would refuse "tmux" outright.
                libc::execvp(program.as_ptr(), argv.as_ptr());
                // execvp only returns on failure.
                libc::_exit(127);
            }
        }

        Ok(Pty { master: master_fd, child: pid })
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: master is a valid fd as long as `self` is alive (Drop closes it).
        let n = unsafe {
            libc::read(self.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: master is a valid fd as long as `self` is alive.
        let n = unsafe {
            libc::write(self.master, buf.as_ptr() as *const libc::c_void, buf.len())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Update the terminal's window size.  TIOCSWINSZ also delivers SIGWINCH
    /// to the foreground process group, so curses-style apps repaint.
    pub fn resize(&mut self, size: TerminalSize) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: master is a valid fd; ioctl is async-signal-safe and the
        // pointer outlives the call.
        let r = unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &ws) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn raw_master(&self) -> RawFd {
        self.master
    }

    pub fn child_pid(&self) -> i32 {
        self.child
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.child > 0 {
            // Polite first: SIGHUP lets shells flush history etc.  If the
            // child does not exit quickly, escalate to SIGKILL.  In all cases
            // we must waitpid() to avoid leaving zombies.
            unsafe {
                libc::kill(self.child, libc::SIGHUP);
            }

            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            let mut reaped = false;
            while std::time::Instant::now() < deadline {
                let mut status: c_int = 0;
                let r = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
                if r > 0 {
                    reaped = true;
                    break;
                }
                if r < 0 {
                    // ECHILD = already reaped by someone else.  Treat as done.
                    reaped = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            if !reaped {
                unsafe {
                    libc::kill(self.child, libc::SIGKILL);
                    let mut status: c_int = 0;
                    libc::waitpid(self.child, &mut status, 0);
                }
            }
            self.child = 0;
        }
        if self.master >= 0 {
            unsafe {
                libc::close(self.master);
            }
            self.master = -1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Read from PTY until `needle` appears in the accumulated bytes, or until
    /// `timeout` elapses, or until EOF.  Used when the child stays alive and
    /// we just want to wait for a specific output to appear.
    fn read_until(pty: &mut Pty, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if acc.windows(needle.len()).any(|w| w == needle) || Instant::now() >= deadline {
                break;
            }
            let mut pfd = libc::pollfd {
                fd: pty.raw_master(),
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, 100) };
            if r < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if r == 0 {
                continue;
            }
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
        acc
    }

    /// Reap a PTY's child process and close the master fd manually.  Used in
    /// tests until the real Drop impl is in place; afterwards this becomes a
    /// no-op safety net.
    fn force_cleanup(pty: &mut Pty) {
        if pty.child > 0 {
            unsafe {
                libc::kill(pty.child, libc::SIGKILL);
                let mut status: c_int = 0;
                libc::waitpid(pty.child, &mut status, 0);
            }
            pty.child = 0;
        }
        if pty.master >= 0 {
            unsafe {
                libc::close(pty.master);
            }
            pty.master = -1;
        }
    }

    /// Read from PTY until EOF or until `timeout` elapses, polling so the
    /// test never hangs.  Returns whatever bytes arrived.
    fn drain_until_eof_or_timeout(pty: &mut Pty, timeout: Duration) -> Vec<u8> {
        let mut output = Vec::new();
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: pty.raw_master(),
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, remaining_ms.max(1)) };
            if r < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if r == 0 {
                continue; // timed out on this poll, loop will check deadline
            }
            match pty.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // EIO is what macOS gives when the slave side is fully closed;
                // treat it as EOF.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
        output
    }

    #[test]
    fn spawn_echo_outputs_message() {
        let mut pty = Pty::spawn(PtyConfig {
            program: "/bin/echo".into(),
            args: vec!["hello marspot".into()],
            size: TerminalSize::default(),
            argv0: None,
        })
        .expect("spawn /bin/echo");

        let output = drain_until_eof_or_timeout(&mut pty, Duration::from_secs(2));
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains("hello marspot"), "expected 'hello marspot' in output, got: {:?}", s);
    }

    #[test]
    fn spawn_returns_live_child_pid() {
        let mut pty = Pty::spawn(PtyConfig {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
            size: TerminalSize::default(),
            argv0: None,
        })
        .expect("spawn /bin/sleep");

        let pid = pty.child_pid();
        assert!(pid > 0, "expected positive pid, got {}", pid);

        // kill -0 probes existence without signaling.  Returning 0 means the
        // process exists and we have permission to signal it.
        let probe = unsafe { libc::kill(pid, 0) };
        assert_eq!(
            probe,
            0,
            "expected child {} to be alive, kill -0 returned {} (errno {:?})",
            pid,
            probe,
            io::Error::last_os_error()
        );

        force_cleanup(&mut pty);
    }

    /// Read back the kernel's idea of the pty's winsize via the master fd —
    /// works on either side of the pty pair since they share the struct.
    fn read_winsize(fd: RawFd) -> libc::winsize {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        assert_eq!(r, 0, "TIOCGWINSZ failed: {}", io::Error::last_os_error());
        ws
    }

    #[test]
    fn resize_propagates_winsize() {
        let mut pty = Pty::spawn(PtyConfig {
            program: "/bin/sleep".into(),
            args: vec!["60".into()],
            size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            argv0: None,
        })
        .expect("spawn /bin/sleep");

        let initial = read_winsize(pty.raw_master());
        assert_eq!(
            (initial.ws_col, initial.ws_row),
            (80, 24),
            "initial winsize wrong"
        );

        pty.resize(TerminalSize {
            cols: 132,
            rows: 50,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");

        let after = read_winsize(pty.raw_master());
        assert_eq!(
            (after.ws_col, after.ws_row),
            (132, 50),
            "resize did not propagate"
        );

        force_cleanup(&mut pty);
    }

    #[test]
    fn write_to_child_round_trips() {
        let mut pty = Pty::spawn(PtyConfig {
            program: "/bin/cat".into(),
            args: vec![],
            size: TerminalSize::default(),
            argv0: None,
        })
        .expect("spawn /bin/cat");

        let n = pty.write(b"ping\n").expect("write");
        assert_eq!(n, 5);

        let acc = read_until(&mut pty, b"ping", Duration::from_secs(2));
        let s = String::from_utf8_lossy(&acc);
        assert!(s.contains("ping"), "expected 'ping' to round-trip via /bin/cat, got: {:?}", s);

        force_cleanup(&mut pty);
    }

    // ----- soak tests -----
    //
    // These run 1000+ spawn/drop cycles to verify Pty doesn't leak fds, child
    // processes, or memory.  They are #[ignore]'d so `cargo test` stays fast;
    // run them explicitly via `bin/soak.sh` or:
    //   cargo test pty::tests::soak_ -- --ignored --test-threads=1

    /// Number of file descriptors currently open by this process.  /dev/fd
    /// on macOS lists this process's fds; the readdir itself uses one fd
    /// briefly, but that offset cancels when we diff before/after.
    fn count_open_fds() -> usize {
        std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
    }

    /// Current resident set size in bytes via proc_pidinfo.  Unlike
    /// getrusage's ru_maxrss (peak), this returns the live value so we can
    /// detect a process that grew and stayed grown.
    fn current_rss_bytes() -> u64 {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_taskinfo>() as i32,
            )
        };
        assert!(r > 0, "proc_pidinfo failed: {}", io::Error::last_os_error());
        info.pti_resident_size
    }

    fn quick_spawn() -> Pty {
        Pty::spawn(PtyConfig {
            program: "/usr/bin/true".into(),
            args: vec![],
            size: TerminalSize::default(),
            argv0: None,
        })
        .expect("spawn /usr/bin/true")
    }

    const SOAK_ITERATIONS: usize = 1000;

    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_no_fd_leak_over_1000_spawns() {
        // Warm up so any one-time allocator/library opens are absorbed into
        // the baseline rather than counted as growth.
        for _ in 0..20 {
            drop(quick_spawn());
        }
        let baseline = count_open_fds();
        for _ in 0..SOAK_ITERATIONS {
            drop(quick_spawn());
        }
        let after = count_open_fds();
        let delta = after.saturating_sub(baseline);
        assert!(
            delta <= 1,
            "fd leak: baseline {} -> after {} ({} delta) over {} iters",
            baseline,
            after,
            delta,
            SOAK_ITERATIONS
        );
    }

    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_no_unreaped_children_over_1000_spawns() {
        for _ in 0..SOAK_ITERATIONS {
            drop(quick_spawn());
        }
        // After Drop, every child must already be reaped.  waitpid(-1, WNOHANG)
        // returning -1 (with errno=ECHILD) means "no children" — the success
        // case.  Any positive return is an unreaped zombie.
        let mut status: c_int = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if r > 0 {
            panic!(
                "found unreaped child pid={} after {} spawns",
                r, SOAK_ITERATIONS
            );
        }
    }

    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_no_memory_growth_over_1000_spawns() {
        // Warm up the allocator's arena pool — first few spawns trigger
        // one-time mappings we don't want to count as a leak.
        for _ in 0..50 {
            drop(quick_spawn());
        }
        let baseline = current_rss_bytes();
        for _ in 0..SOAK_ITERATIONS {
            drop(quick_spawn());
        }
        let after = current_rss_bytes();

        // 5 MB tolerance: macOS allocator keeps some arenas mapped post-free.
        // A real per-iter leak of even 5 KB would surface as 5 MB across 1000.
        const TOLERANCE_BYTES: u64 = 5 * 1024 * 1024;
        let growth = after.saturating_sub(baseline);
        assert!(
            growth <= TOLERANCE_BYTES,
            "RSS grew {} bytes (baseline {} -> after {}) over {} iters; tolerance {} bytes",
            growth,
            baseline,
            after,
            SOAK_ITERATIONS,
            TOLERANCE_BYTES
        );
    }

    // ----- end soak tests -----

    #[test]
    fn drop_kills_child_and_closes_fd() {
        let pid;
        let master_fd;
        {
            let pty = Pty::spawn(PtyConfig {
                program: "/bin/sleep".into(),
                args: vec!["60".into()],
                size: TerminalSize::default(),
            argv0: None,
            })
            .expect("spawn /bin/sleep");

            pid = pty.child_pid();
            master_fd = pty.raw_master();

            // Pre-condition: child is alive and fd is valid.
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child {} should be alive", pid);
            assert_ne!(unsafe { libc::fcntl(master_fd, libc::F_GETFD) }, -1, "fd should be open");
        } // Drop runs here

        // Give the kernel a beat to finish reaping.  100ms is generous; SIGHUP
        // on /bin/sleep terminates immediately so we expect well under that.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Child must be gone.
        let probe = unsafe { libc::kill(pid, 0) };
        let err = io::Error::last_os_error();
        assert_eq!(probe, -1, "child {} should be reaped", pid);
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ESRCH),
            "expected ESRCH after Drop, got {:?}",
            err
        );

        // Master fd must be closed.
        let r = unsafe { libc::fcntl(master_fd, libc::F_GETFD) };
        let fd_err = io::Error::last_os_error();
        assert_eq!(r, -1, "master fd should be closed");
        assert_eq!(
            fd_err.raw_os_error(),
            Some(libc::EBADF),
            "expected EBADF on closed fd, got {:?}",
            fd_err
        );
    }

    #[test]
    fn read_after_child_exits_returns_eof() {
        let mut pty = Pty::spawn(PtyConfig {
            program: "/usr/bin/true".into(),
            args: vec![],
            size: TerminalSize::default(),
            argv0: None,
        })
        .expect("spawn /usr/bin/true");

        // /usr/bin/true exits immediately; drain must reach EOF/EIO well before
        // the timeout — this is the real assertion (no infinite blocking).
        let start = Instant::now();
        let _ = drain_until_eof_or_timeout(&mut pty, Duration::from_secs(3));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "expected EOF within 2s of /usr/bin/true exit, took {:?}",
            start.elapsed()
        );

        // After EOF, subsequent reads must report end-of-stream cleanly,
        // not block.  macOS reports EIO once the slave side is fully closed.
        let mut buf = [0u8; 16];
        match pty.read(&mut buf) {
            Ok(0) => {}
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {}
            other => panic!("expected EOF/EIO after child exit, got {:?}", other),
        }
    }
}
