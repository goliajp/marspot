//! Session: one independent terminal instance.
//!
//! A Session owns its own PTY (and the child shell), its own Terminal
//! state machine + grid, and its own bounded mpsc channel fed by a
//! dedicated reader thread.  It is **deliberately ignorant of the
//! caller**: it knows nothing about windows, layouts, sidebars, or
//! event-loop event types.
//!
//! The caller supplies a `wake` callback that the reader thread invokes
//! whenever new bytes are available (or the child has exited).  The
//! caller's event loop then calls [`Session::pump`] to drain those
//! bytes through the parser into the grid.  This decoupling is what
//! lets the same Session power both `mars` (multi-terminal grid app)
//! and `mcli` (single-terminal standalone app).

use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// "Output produced within this window ⇒ Active, else Idle."  Two
/// seconds is short enough to feel live (a `make` finishing pulses
/// the dot) without flickering on every keystroke echo.
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Produced output recently (within ACTIVE_WINDOW).
    Active,
    /// Alive, child process still running, but quiet.
    Idle,
    /// Reader thread saw EOF — child has exited.  Final-frame contents
    /// remain readable but no new bytes will arrive.
    Exited,
}

use crate::pty::{Pty, PtyConfig, TerminalSize};
use crate::terminal::Terminal;

/// Bytes per chunk handed off from the reader thread.  64 KiB is the
/// opportunistic upper bound — for interactive output we send what
/// arrived; for bulk output we keep reading non-blocking until the PTY
/// drains, coalescing into one chunk before incurring the per-event
/// dispatch cost on the caller's main loop.
const READ_BUF: usize = 64 * 1024;

/// Bounded capacity of each session's PTY → caller channel.  When full
/// the reader thread blocks on send, propagating backpressure to the
/// kernel pipe buffer to the child's writes — bounded memory cost no
/// matter how fast the child writes.
const PTY_CHANNEL_CAPACITY: usize = 64;

pub struct Session {
    pub terminal: Terminal,
    pty: Pty,
    rx: Receiver<Vec<u8>>,
    exited: Arc<AtomicBool>,
    /// Wall-clock instant of the most recent byte fed to the terminal.
    /// Drives "recently active" indicators in any UI built on top.
    pub last_output: Option<Instant>,
}

impl Session {
    /// Spawn a new session: forkpty into the user's shell, start a
    /// reader thread that pumps bytes into a bounded channel and calls
    /// `wake` on each chunk and once on EOF.
    ///
    /// The shell is `MARS_SHELL` → `$SHELL` → `/bin/zsh`, in that order.
    /// `wake` typically posts a user event into the caller's event loop
    /// so the main thread comes around to call [`pump`](Self::pump).
    pub fn spawn<W>(cols: u16, rows: u16, wake: W) -> io::Result<Self>
    where
        W: Fn() + Send + Sync + 'static,
    {
        let shell = std::env::var("MARS_SHELL")
            .or_else(|_| std::env::var("SHELL"))
            .unwrap_or_else(|_| "/bin/zsh".into());
        Self::spawn_with(&shell, &[], cols, rows, wake)
    }

    /// Spawn variant with explicit program + args.  Useful for tests
    /// (no env-var racing) and for any future "open this session
    /// running <command>" UI affordance.
    pub fn spawn_with<W>(
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        wake: W,
    ) -> io::Result<Self>
    where
        W: Fn() + Send + Sync + 'static,
    {
        let pty = Pty::spawn(PtyConfig {
            program: program.into(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            size: TerminalSize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            },
        })?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(PTY_CHANNEL_CAPACITY);
        let exited = Arc::new(AtomicBool::new(false));
        spawn_reader(pty.raw_master(), tx, exited.clone(), wake);
        Ok(Self {
            terminal: Terminal::new(cols, rows),
            pty,
            rx,
            exited,
            last_output: None,
        })
    }

    /// Drain whatever bytes the reader thread has queued into this
    /// session's terminal.  Returns the total bytes fed.  Caller should
    /// trigger a redraw when the return value is non-zero.
    pub fn pump(&mut self) -> usize {
        let mut total = 0;
        while let Ok(chunk) = self.rx.try_recv() {
            total += chunk.len();
            self.terminal.feed(&chunk);
        }
        if total > 0 {
            self.last_output = Some(Instant::now());
        }
        total
    }

    /// True after the reader thread has observed EOF on the PTY master
    /// (i.e. the child shell exited).  The terminal's final-frame
    /// contents remain readable; callers may keep displaying them.
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Write keystroke / paste bytes back to the child.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pty.write(bytes)
    }

    /// Resize the terminal grid + the PTY winsize.  The shell receives
    /// SIGWINCH and repaints accordingly.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.resize(cols, rows);
        let _ = self.pty.resize(TerminalSize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Borrow the terminal for reading (used by render).
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Roll up `is_exited()` + `last_output` into a single state suitable
    /// for the sidebar's status dot.  See [`SessionState`].
    pub fn state(&self) -> SessionState {
        if self.is_exited() {
            return SessionState::Exited;
        }
        match self.last_output {
            Some(t) if t.elapsed() < ACTIVE_WINDOW => SessionState::Active,
            _ => SessionState::Idle,
        }
    }
}

fn spawn_reader<W>(master_fd: RawFd, tx: SyncSender<Vec<u8>>, exited: Arc<AtomicBool>, wake: W)
where
    W: Fn() + Send + Sync + 'static,
{
    thread::Builder::new()
        .name("mars-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                let n = unsafe {
                    libc::read(
                        master_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    exited.store(true, Ordering::Release);
                    wake();
                    break;
                }
                let mut total = n as usize;

                // Opportunistic drain: poll(0) and read whatever else is
                // already in the kernel pipe.  Cuts IPC × dispatch cost
                // on bulk output without adding latency to interactive
                // output (single byte sent immediately because poll
                // says "no more").
                while total < buf.len() {
                    let mut pfd = libc::pollfd {
                        fd: master_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
                    if ready <= 0 {
                        break;
                    }
                    let n2 = unsafe {
                        libc::read(
                            master_fd,
                            buf.as_mut_ptr().add(total) as *mut libc::c_void,
                            buf.len() - total,
                        )
                    };
                    if n2 <= 0 {
                        break;
                    }
                    total += n2 as usize;
                }

                let chunk = buf[..total].to_vec();
                if tx.send(chunk).is_err() {
                    break;
                }
                wake();
            }
        })
        .expect("spawn pty reader thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Spin until `pred` returns true or `deadline_ms` elapses.  Pumps
    /// the session each tick so the test sees fresh state.  Returns
    /// the time taken (or panics on timeout).
    fn spin_until<F: FnMut(&mut Session) -> bool>(
        s: &mut Session,
        deadline_ms: u64,
        mut pred: F,
    ) -> Duration {
        let start = std::time::Instant::now();
        loop {
            s.pump();
            if pred(s) {
                return start.elapsed();
            }
            if start.elapsed() > Duration::from_millis(deadline_ms) {
                panic!("spin_until timed out after {} ms", deadline_ms);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn first_row(s: &Session) -> String {
        let g = s.terminal.grid();
        (0..g.cols()).map(|c| g.cell(c, 0).ch).collect()
    }

    /// Concatenate every row of the visible grid into one string.
    /// Useful for tests that don't care which line a substring landed
    /// on (PTY echo + shell stdout can split a single round-trip
    /// across rows).
    fn all_rows(s: &Session) -> String {
        let g = s.terminal.grid();
        let mut out = String::new();
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                out.push(g.cell(c, r).ch);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn spawn_with_runs_to_eof_and_marks_exited() {
        // `/bin/sh -c 'true'` exits immediately; the reader thread sees
        // EOF, sets the exited flag, and fires the wake callback.
        let woke = Arc::new(AtomicUsize::new(0));
        let woke_clone = woke.clone();
        let mut s = Session::spawn_with(
            "/bin/sh",
            &["-c", "true"],
            40,
            10,
            move || {
                woke_clone.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("spawn");

        spin_until(&mut s, 1000, |s| s.is_exited());
        assert!(s.is_exited());
        // EOF wake always fires; reader-thread chunks may add more.
        assert!(woke.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn pump_feeds_bytes_into_terminal_grid() {
        let mut s = Session::spawn_with(
            "/bin/sh",
            &["-c", "printf hello && exit"],
            40,
            10,
            || {},
        )
        .expect("spawn");

        spin_until(&mut s, 1000, |s| first_row(s).starts_with("hello"));
        assert!(first_row(&s).starts_with("hello"));
    }

    #[test]
    fn write_round_trips_through_pty() {
        // `stty -echo` so PTY won't echo our input back as a separate
        // visible line; output ends up with just shell's printf result.
        let mut s = Session::spawn_with(
            "/bin/sh",
            &["-c", "stty -echo; read line; printf '<%s>' \"$line\""],
            40,
            10,
            || {},
        )
        .expect("spawn");

        // Give stty time to apply before we write.
        std::thread::sleep(Duration::from_millis(50));
        s.write(b"sentinel\n").expect("write");

        spin_until(&mut s, 1500, |s| all_rows(s).contains("<sentinel>"));
        assert!(all_rows(&s).contains("<sentinel>"));
    }

    #[test]
    fn pump_returns_zero_when_no_data_pending() {
        // Use `sleep` so the child stays alive but never writes.
        let mut s = Session::spawn_with("/bin/sh", &["-c", "sleep 5"], 40, 10, || {})
            .expect("spawn");

        // Give the reader thread a moment to settle, then pump should
        // find nothing in the channel.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(s.pump(), 0);
        assert!(!s.is_exited());

        // Drop the session — Pty::drop reaps the sleep child.
        drop(s);
    }

    #[test]
    fn resize_propagates_to_terminal_grid() {
        let mut s = Session::spawn_with("/bin/sh", &["-c", "sleep 5"], 40, 10, || {})
            .expect("spawn");
        assert_eq!(s.terminal.grid().cols(), 40);
        assert_eq!(s.terminal.grid().rows(), 10);
        s.resize(60, 20);
        assert_eq!(s.terminal.grid().cols(), 60);
        assert_eq!(s.terminal.grid().rows(), 20);
    }
}
