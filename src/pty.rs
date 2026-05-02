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

#[derive(Clone, Debug)]
pub struct PtyConfig {
    pub program: String,
    pub args: Vec<String>,
    pub size: TerminalSize,
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
        let arg_cstrings: Vec<CString> = config
            .args
            .iter()
            .map(|a| CString::new(a.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"))?;

        // argv[0] = program, argv[1..] = args, argv[last] = NULL
        let mut argv: Vec<*const c_char> = Vec::with_capacity(arg_cstrings.len() + 2);
        argv.push(program.as_ptr());
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
                libc::execv(program.as_ptr(), argv.as_ptr());
                // execv only returns on failure.
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

    pub fn raw_master(&self) -> RawFd {
        self.master
    }

    pub fn child_pid(&self) -> i32 {
        self.child
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // implemented in a later test cycle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
            args: vec!["hello mars".into()],
            size: TerminalSize::default(),
        })
        .expect("spawn /bin/echo");

        let output = drain_until_eof_or_timeout(&mut pty, Duration::from_secs(2));
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains("hello mars"), "expected 'hello mars' in output, got: {:?}", s);
    }
}
