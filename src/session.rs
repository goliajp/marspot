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
use std::time::Instant;

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
        let pty = Pty::spawn(PtyConfig {
            program: shell,
            args: Vec::new(),
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
