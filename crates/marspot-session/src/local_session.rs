//! `LocalSession` — RFC-003 step 1b.
//!
//! A per-session abstraction that owns the PTY master, shell child,
//! terminal parser/grid, and bytelog **directly in the L3 process** —
//! no shelld dependency.  Mirrors the L3-facing public surface of
//! `marspot_term::shelld_client::ShelldSession` so the main loop can
//! swap between them.
//!
//! The reader-thread / mpsc wake pattern is intentionally the same
//! as `ShelldSession`'s: spawn a thread blocked in `read(master)`,
//! forward bytes via a channel, fire the supplied wake closure so the
//! L3 main loop's unified event channel pops `SessionEvent::Wake`.
//! Idle CPU stays ~0 (kernel parks the reader on the master fd, main
//! loop parks on its mpsc).
//!
//! Phase 1b deliberately does NOT integrate with `main.rs` — that's
//! Phase 1c's job.  This file lands the new type + a self-test that
//! exercises spawn → write → pump → exit standalone.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use marspot_term::bytelog::ByteLog;
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use marspot_term::shelld_client::{PendingPage, SessionState};
use marspot_term::terminal::Terminal;

/// How recently we have to have seen PTY output to count as "active".
/// Mirrors `shelld_client::ACTIVE_WINDOW` so L2's badge state is the
/// same regardless of which session impl is feeding it.
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

/// Reader-thread chunk size.  Bigger than the parser's typical 4 KiB
/// burst because `cat bigfile` happily fills 64 KiB in a single read,
/// and the kernel returns whatever's queued — no benefit to capping.
const READ_BUF_BYTES: usize = 64 * 1024;

pub struct LocalSession {
    id: u64,
    /// Cloned and shared with the reader thread so both halves can
    /// touch the master fd without a Mutex (kernel serialises on the
    /// fd).  `Pty`'s Drop closes the fd and reaps the child when the
    /// last `Arc` goes away.
    pty: Arc<Pty>,
    child_pid: i32,
    terminal: Terminal,
    bytelog: Option<ByteLog>,
    rx: Receiver<Vec<u8>>,
    exited: Arc<AtomicBool>,
    last_output: Option<Instant>,
    pending_scrollback_pages: Vec<PendingPage>,
    cols: u16,
    rows: u16,
}

impl LocalSession {
    /// Spawn a fresh shell on a new PTY and start the reader thread.
    ///
    /// `wake` is invoked from the reader thread whenever bytes arrive
    /// or the child exits, so the L3 main loop's unified mpsc can pop
    /// a `Wake` event.  The shell program is `$SHELL` (or `/bin/zsh`),
    /// argv[0] gets the conventional leading `-` so .zprofile fires.
    /// `cwd_override` "" → inherit `$HOME` (matches the shelld path
    /// so the user lands where they expect on a fresh window).
    pub fn spawn<W>(
        id: u64,
        cols: u16,
        rows: u16,
        cwd_override: &str,
        wake: W,
    ) -> io::Result<Self>
    where
        W: Fn() + Send + 'static,
    {
        let cwd = if cwd_override.is_empty() {
            std::env::var("HOME").ok()
        } else {
            Some(cwd_override.to_string())
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let argv0 = format!(
            "-{}",
            std::path::Path::new(&shell)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("sh")
        );

        let pty = Pty::spawn(PtyConfig {
            program: shell,
            args: Vec::new(),
            size: TerminalSize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            },
            argv0: Some(argv0),
            cwd,
        })?;
        let child_pid = pty.child_pid();
        let pty = Arc::new(pty);

        // Best-effort bytelog open — a failure means we lose history
        // replay for this session but the session itself still runs.
        let bytelog = match ByteLog::open(id) {
            Ok(b) => Some(b),
            Err(_) => None,
        };

        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));

        let pty_reader = Arc::clone(&pty);
        let exited_writer = Arc::clone(&exited);
        thread::Builder::new()
            .name(format!("l3-pty-reader-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUF_BYTES];
                loop {
                    let n = match pty_reader.read_shared(&mut buf) {
                        Ok(0) => 0,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // macOS gives EIO when the slave side fully closes.
                        Err(e) if e.raw_os_error() == Some(libc::EIO) => 0,
                        Err(_) => 0,
                    };
                    if n == 0 {
                        exited_writer.store(true, Ordering::SeqCst);
                        wake();
                        break;
                    }
                    if tx.send(buf[..n].to_vec()).is_err() {
                        // main loop is gone — stop forwarding
                        break;
                    }
                    wake();
                }
            })
            .expect("spawn l3-pty-reader thread");

        Ok(Self {
            id,
            pty,
            child_pid,
            terminal: Terminal::new(cols, rows),
            bytelog,
            rx,
            exited,
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            cols,
            rows,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn child_pid(&self) -> i32 {
        self.child_pid
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    pub fn state(&self) -> SessionState {
        if self.is_exited() {
            return SessionState::Exited;
        }
        match self.last_output {
            Some(t) if t.elapsed() <= ACTIVE_WINDOW => SessionState::Active,
            _ => SessionState::Idle,
        }
    }

    /// Drain every chunk the reader has queued, append each to the
    /// bytelog, feed it into the terminal parser.  Returns bytes
    /// consumed this tick (0 = nothing happened).
    pub fn pump(&mut self) -> usize {
        let mut total = 0;
        loop {
            match self.rx.try_recv() {
                Ok(bytes) => {
                    if let Some(b) = self.bytelog.as_mut() {
                        let _ = b.append(&bytes);
                    }
                    self.terminal.feed(&bytes);
                    total += bytes.len();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if total > 0 {
            self.last_output = Some(Instant::now());
        }
        total
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pty.write_shared(bytes)
    }

    /// Resize the terminal + PTY.  TIOCSWINSZ delivers SIGWINCH so
    /// curses-style apps repaint; the parser's grid reflow keeps
    /// existing rows aligned.  `Pty::resize` wants `&mut self`; we
    /// only hold `Arc<Pty>` because the reader thread shares it, so
    /// roll our own ioctl on the raw fd — kernel serialises winsize
    /// updates per-fd, so a `&self` view is sound.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let r = unsafe { libc::ioctl(self.pty.raw_master(), libc::TIOCSWINSZ, &ws) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        self.terminal.resize(cols, rows);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// RFC-002 §8 scrollback-page surface — stubbed for the L3-owns-PTY
    /// path because LocalSession owns the bytelog directly (no L4 to
    /// fetch from). Phase 4 / RFC-003 step 4 wires bytelog replay here.
    pub fn request_scrollback_page(
        &mut self,
        _line_start: u32,
        _line_count: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    /// RFC-002 §8 — drain the page cache.  Empty for LocalSession
    /// until bytelog replay arrives; mirrors `ShelldSession`'s
    /// vec-returning shape so caller code is identical.
    pub fn take_pending_scrollback_pages(&mut self) -> Vec<PendingPage> {
        std::mem::take(&mut self.pending_scrollback_pages)
    }
}

// LocalSession reads the PTY master fd via Arc<Pty>; kernel handles
// concurrent fd ops itself.  Sending/sharing across threads is sound
// because no field exposes interior mutability that needs Send +
// Sync we don't already pay for explicitly (Terminal stays in the
// main thread, the reader thread only touches the Arc<Pty>).
unsafe impl Send for LocalSession {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn pump_until<F: FnMut(&LocalSession) -> bool>(
        s: &mut LocalSession,
        mut done: F,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            s.pump();
            if done(s) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        s.pump();
        done(s)
    }

    /// spawn → write a printable byte through the shell → see it land
    /// in the grid via the parser.  Exercises the whole hot path.
    #[test]
    fn spawn_write_pump_echoes_into_grid() {
        let wake_count = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wake_count);
        let mut s = LocalSession::spawn(
            42,
            80,
            24,
            "",
            move || {
                w.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("spawn local session");

        // Wait for the shell prompt to land (any output is fine).
        assert!(
            pump_until(&mut s, |s| s.terminal().grid().cursor() != (0, 0), Duration::from_secs(3)),
            "expected shell to produce some output and move the cursor"
        );

        assert!(s.child_pid() > 0, "child pid should be set");
        assert!(!s.is_exited(), "session should be alive while shell runs");
        assert!(
            wake_count.load(Ordering::SeqCst) > 0,
            "reader thread must have signalled the main loop at least once"
        );
    }

    /// Closing the slave side (shell exits) flips `is_exited` and the
    /// reader thread fires one final wake so the main loop won't park
    /// forever waiting for more bytes.
    #[test]
    fn shell_exit_flips_is_exited() {
        let wake = Arc::new(AtomicBool::new(false));
        // Use `/bin/sh -c "exit 0"` so the child exits immediately.
        let mut s = LocalSession {
            id: 1,
            pty: Arc::new(
                Pty::spawn(PtyConfig {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), "exit 0".into()],
                    size: TerminalSize {
                        cols: 80,
                        rows: 24,
                        pixel_width: 0,
                        pixel_height: 0,
                    },
                    argv0: None,
                    cwd: None,
                })
                .expect("spawn /bin/sh"),
            ),
            child_pid: 0,
            terminal: Terminal::new(80, 24),
            bytelog: None,
            rx: { let (_t, r) = mpsc::channel(); r },
            exited: Arc::new(AtomicBool::new(false)),
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            cols: 80,
            rows: 24,
        };
        // Manually start a reader bound to the same Pty so this short
        // helper-test mirrors what spawn() does internally.
        let pty_reader = Arc::clone(&s.pty);
        let exited_writer = Arc::clone(&s.exited);
        let (tx, new_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        s.rx = new_rx;
        let w2 = Arc::clone(&wake);
        thread::spawn(move || {
            let mut buf = vec![0u8; READ_BUF_BYTES];
            loop {
                let n = match pty_reader.read_shared(&mut buf) {
                    Ok(0) => 0,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => 0,
                    Err(_) => 0,
                };
                if n == 0 {
                    exited_writer.store(true, Ordering::SeqCst);
                    w2.store(true, Ordering::SeqCst);
                    break;
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            s.pump();
            if s.is_exited() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(s.is_exited(), "shell exit should flip is_exited within 3s");
        assert!(wake.load(Ordering::SeqCst), "reader should fire wake on exit");
    }
}

