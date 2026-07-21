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
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use marspot_term::bytelog::ByteLog;
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use marspot_term::session_state::{PendingPage, SessionState};
use marspot_term::terminal::Terminal;

/// How recently we have to have seen PTY output to count as "active".
/// Mirrors `shelld_client::ACTIVE_WINDOW` so L2's badge state is the
/// same regardless of which session impl is feeding it.
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

/// Reader-thread chunk size.  Bigger than the parser's typical 4 KiB
/// burst because `cat bigfile` happily fills 64 KiB in a single read,
/// and the kernel returns whatever's queued — no benefit to capping.
const READ_BUF_BYTES: usize = 64 * 1024;

/// Write half of the PTY, owned by a dedicated thread.
///
/// **Why this exists.**  `write(2)` on a PTY master blocks once the
/// tty's input buffer fills, and it fills whenever the foreground
/// process stops reading stdin.  Calling it from the L3 main loop —
/// which is what we used to do — parks that whole loop: no PTY output
/// gets pumped, no control frames get served, no snapshot gets taken,
/// for as long as the child stays busy.  Symptom: one pane goes
/// completely unresponsive for minutes and then recovers on its own,
/// while every other pane (a separate L3 process) stays fine.
///
/// Moving the write behind a thread that is *allowed* to block gives
/// the main loop a non-blocking `write()` and keeps idle CPU at zero
/// (the thread parks in `recv`, not a spin loop).  It mirrors the
/// reader-thread half that has always been here.
struct PtyWriter {
    tx: Sender<Vec<u8>>,
    /// Bytes handed to the thread but not yet accepted by the kernel.
    /// Shared so `write` can enforce the cap without a round-trip.
    pending: Arc<AtomicUsize>,
    /// Bytes refused because the queue was full.  Monotonic; read for
    /// logging so a stall is visible in the field instead of silent.
    dropped: Arc<AtomicUsize>,
}

/// Spawn the writer thread for `pty` and return its queue handle.
fn spawn_pty_writer(id: u64, pty: Arc<Pty>) -> PtyWriter {
    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
    let pending = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let pending_thread = Arc::clone(&pending);
    thread::Builder::new()
        .name(format!("l3-pty-writer-{id}"))
        .spawn(move || {
            // Ends when the session drops its Sender.
            while let Ok(chunk) = rx.recv() {
                let mut off = 0;
                while off < chunk.len() {
                    match pty.write_shared(&chunk[off..]) {
                        // Short writes are normal on a tty whose buffer
                        // is nearly full — the old inline call ignored
                        // the return value entirely and silently lost
                        // the tail of anything it didn't accept.
                        Ok(0) => break,
                        Ok(n) => off += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // Dead fd (child gone).  Abandon this chunk and
                        // keep draining so `pending` stays truthful and
                        // the session tears down through the normal
                        // exit path rather than wedging here.
                        Err(_) => break,
                    }
                }
                pending_thread.fetch_sub(chunk.len(), Ordering::SeqCst);
            }
        })
        .expect("spawn l3-pty-writer thread");
    PtyWriter { tx, pending, dropped }
}

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
    /// PTY write half.  Dropping this ends the writer thread.
    writer: PtyWriter,
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

        // RFC-003 §6 Amendment 13 — restore the TERM/COLORTERM/ZDOTDIR
        // env setup that the deleted L4 shelld used to do.
        //
        // GUI-launched marspot.app inherits a minimal env from
        // launchd; TERM is commonly absent or `network`, which makes
        // zsh+terminfo derive a dumb terminal capability set.  Two
        // visible regressions when that happens:
        //   * backward-delete-char (Backspace) emits a bare ` ` instead
        //     of `\b \b`, so the deleted column appears blanked but
        //     the cursor stays put → display looks like "added a
        //     space, kept old digits".
        //   * COLORTERM unset → apps downgrade truecolor to the
        //     256-cube; SGR 38;2;r;g;b approximations are wrong hues
        //     (claudecode coral orange → rose pink, ls --color labels
        //     visibly drift), and most output collapses to plain
        //     white.  marspot's parser does handle 38;2;r;g;b
        //     truecolor — we just have to advertise it.
        //
        // L4 shelld set both of these on its own startup so every
        // forked zsh saw a sane env.  We replicate the same heuristic
        // here at the L3 boundary now that L3 owns the spawn.  Same
        // for the ZDOTDIR shim that installs PROMPT_SP / EOL_MARK.
        let term_ok = std::env::var("TERM")
            .map(|t| !t.is_empty() && t != "network" && t != "dumb" && t != "unknown")
            .unwrap_or(false);
        if !term_ok {
            unsafe { std::env::set_var("TERM", "xterm-256color") };
        }
        if std::env::var_os("COLORTERM").is_none() {
            unsafe { std::env::set_var("COLORTERM", "truecolor") };
        }
        marspot_term::session::ensure_zdot_shim_for_external_shells();

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
            // L2 → L3 plumbing vars (MARSPOT_SESSION_ID, MARSPOT_SHM_FD,
            // MARSPOT_L3_OWNS_PTY, …) are for THIS process, not the
            // user's shell.  Leaking them broke real workflows: a
            // `cargo test` inside a marspot pane inherited
            // MARSPOT_SESSION_ID and overwrote that session's on-disk
            // scrollback (2026-07-03).  L3's own env is untouched, so
            // the self-execv update path still reads them.
            env_remove_prefixes: vec!["MARSPOT_".into()],
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

        let writer = spawn_pty_writer(id, Arc::clone(&pty));

        Ok(Self {
            id,
            pty,
            child_pid,
            terminal: Terminal::new(cols, rows),
            bytelog,
            rx,
            writer,
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

    /// Queue `bytes` for the PTY.  Never blocks — see `PtyWriter`.
    ///
    /// `Ok(n)` means queued, not delivered; the only ordering promise
    /// is FIFO, which is all the callers (keystrokes, paste, injected
    /// input) need.  `WouldBlock` means the child has not read its
    /// stdin for long enough to fill the queue and the input was
    /// dropped — callers log it rather than retrying, since retrying
    /// into a full queue just drops again.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let pending = self.writer.pending.load(Ordering::SeqCst);
        if pending + bytes.len() > marspot_term::frame_writer::cap::PTY {
            let dropped = self.writer.dropped.fetch_add(bytes.len(), Ordering::SeqCst)
                + bytes.len();
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "pty write queue full: {pending} B pending, {dropped} B dropped so far \
                     (foreground process is not reading stdin)"
                ),
            ));
        }
        self.writer.pending.fetch_add(bytes.len(), Ordering::SeqCst);
        if self.writer.tx.send(bytes.to_vec()).is_err() {
            self.writer.pending.fetch_sub(bytes.len(), Ordering::SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty writer thread gone",
            ));
        }
        Ok(bytes.len())
    }

    /// Bytes queued for the PTY but not yet accepted by it.  Non-zero
    /// for more than a moment means the foreground process is not
    /// reading its stdin.
    pub fn pty_write_backlog(&self) -> usize {
        self.writer.pending.load(Ordering::SeqCst)
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

    /// RFC-003 §6 Amendment 14 — L3 execv self-update support.
    ///
    /// Reconstruct a LocalSession on top of an already-running PTY +
    /// shell child that we inherited across `execv`.  No fork, no
    /// `Pty::spawn` — the pair (master_fd, child_pid) is handed in by
    /// the pre-execv image via the handoff env vars; PID is preserved
    /// across `execv` so the shell at the other end of the master fd
    /// keeps living through the image swap.
    ///
    /// The Terminal value carries the serialized grid/cursor/mode
    /// state the new image must adopt (so the L2 mirror doesn't blink
    /// to an empty grid between the swap and the next PTY burst).
    /// Bytelog is re-opened by id — the file on disk is untouched.
    ///
    /// Mirrors the reader-thread shape of `spawn` exactly so the rest
    /// of the L3 main loop is identical after this returns.
    pub fn from_handoff<W>(
        id: u64,
        master_fd: RawFd,
        child_pid: i32,
        cols: u16,
        rows: u16,
        terminal: Terminal,
        wake: W,
    ) -> io::Result<Self>
    where
        W: Fn() + Send + 'static,
    {
        let pty = Pty::from_raw_master(master_fd, child_pid);
        let pty = Arc::new(pty);

        // Re-open the bytelog at append.  Pre-execv writes already
        // landed on disk; the new image appends from here on.
        let bytelog = ByteLog::open(id).ok();

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
                        Err(e) if e.raw_os_error() == Some(libc::EIO) => 0,
                        Err(_) => 0,
                    };
                    if n == 0 {
                        exited_writer.store(true, Ordering::SeqCst);
                        wake();
                        break;
                    }
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .expect("spawn l3-pty-reader thread");

        let writer = spawn_pty_writer(id, Arc::clone(&pty));

        Ok(Self {
            id,
            pty,
            child_pid,
            terminal,
            bytelog,
            rx,
            writer,
            exited,
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            cols,
            rows,
        })
    }

    /// RFC-003 §6 Amendment 14 — disassemble for `execv` handoff.
    ///
    /// Returns the bits the new image needs to reconstruct via
    /// `from_handoff`: (master_fd, child_pid, terminal, cols, rows).
    ///
    /// `mem::forget`s the rest of Self so:
    ///   * `Pty::Drop` does NOT SIGHUP the shell child — the running
    ///     zsh must survive the image swap, that's the whole point.
    ///   * The reader thread's `Arc<Pty>` + `tx` are orphaned — the
    ///     subsequent `execv` kills the thread and discards the heap,
    ///     so no real leak (the OS reclaims everything).
    ///
    /// Caller should `pump()` once right before this so any bytes the
    /// reader has already queued are folded into the Terminal that
    /// gets serialized; otherwise those bytes are lost in the swap.
    pub fn extract_for_handoff(self) -> (RawFd, i32, Terminal, u16, u16) {
        let master_fd = self.pty.raw_master();
        let child_pid = self.child_pid;
        let cols = self.cols;
        let rows = self.rows;
        // SAFETY: `ptr::read` the Terminal out, then `mem::forget` the
        // surrounding Self so its Drop (which would drop everything,
        // including the duplicate Terminal we just moved out) never
        // runs.  Standard "destructure a !Copy struct without Drop"
        // pattern.
        let terminal = unsafe {
            let t = std::ptr::read(&self.terminal);
            std::mem::forget(self);
            t
        };
        (master_fd, child_pid, terminal, cols, rows)
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
    /// Regression: a child that never reads its stdin must not be able
    /// to stall the main loop.
    ///
    /// The child must be in **raw** mode for this to reproduce, and
    /// that detail is the whole bug.  Measured on macOS 15:
    ///
    /// | child tty mode | 256 KiB write to the master        |
    /// |----------------|-----------------------------------|
    /// | canonical      | 0.011 s — the tty discards overflow |
    /// | raw            | accepts 1022 B, then **blocks**     |
    ///
    /// So a pane sitting at a shell prompt never showed this, while a
    /// pane running a TUI (claudecode, vim, anything that raws the tty)
    /// froze the moment its foreground process stopped reading stdin —
    /// which is any long tool call — and unfroze when it resumed.  A
    /// canonical-mode child would let this test pass against the old
    /// inline write, so don't "simplify" the `stty raw` away.
    #[test]
    fn write_does_not_block_on_a_child_that_ignores_stdin() {
        let pty_arc = Arc::new(
            Pty::spawn(PtyConfig {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "stty raw -echo; sleep 3".into()],
                size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
                argv0: None,
                cwd: None,
                ..Default::default()
            })
            .expect("spawn /bin/sh"),
        );
        let mut s = LocalSession {
            id: 1,
            pty: Arc::clone(&pty_arc),
            child_pid: 0,
            terminal: Terminal::new(80, 24),
            bytelog: None,
            rx: { let (_t, r) = mpsc::channel(); r },
            writer: spawn_pty_writer(1, Arc::clone(&pty_arc)),
            exited: Arc::new(AtomicBool::new(false)),
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            cols: 80,
            rows: 24,
        };

        // Let the child reach `stty raw` — before that it's canonical
        // and the kernel would swallow everything without blocking.
        thread::sleep(Duration::from_millis(400));

        // 128 KiB — far past the ~1 KiB raw-mode buffer, still under
        // the queue cap.
        let chunk = vec![b'x'; 4096];
        let t0 = Instant::now();
        let mut queued = 0usize;
        for _ in 0..32 {
            if s.write(&chunk).is_ok() {
                queued += chunk.len();
            }
        }
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "write blocked for {elapsed:?} — the queue is not absorbing a non-reading child"
        );
        assert_eq!(queued, 32 * 4096, "everything under the cap should queue");
        // Proof the child really isn't draining: the kernel cannot have
        // taken all of it, so bytes must still be sitting in the queue.
        assert!(
            s.pty_write_backlog() > 0,
            "expected a backlog against a child that never reads stdin"
        );
    }

    /// The queue is bounded: past the cap, input is refused rather than
    /// accumulating without limit.
    #[test]
    fn write_queue_is_bounded() {
        let pty_arc = Arc::new(
            Pty::spawn(PtyConfig {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "stty raw -echo; sleep 3".into()],
                size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
                argv0: None,
                cwd: None,
                ..Default::default()
            })
            .expect("spawn /bin/sh"),
        );
        let mut s = LocalSession {
            id: 2,
            pty: Arc::clone(&pty_arc),
            child_pid: 0,
            terminal: Terminal::new(80, 24),
            bytelog: None,
            rx: { let (_t, r) = mpsc::channel(); r },
            writer: spawn_pty_writer(2, Arc::clone(&pty_arc)),
            exited: Arc::new(AtomicBool::new(false)),
            last_output: None,
            pending_scrollback_pages: Vec::new(),
            cols: 80,
            rows: 24,
        };

        thread::sleep(Duration::from_millis(400));
        let chunk = vec![b'x'; 16 * 1024];
        let mut refused = false;
        // 64 × 16 KiB = 1 MiB against a 256 KiB cap.
        for _ in 0..64 {
            if let Err(e) = s.write(&chunk) {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                refused = true;
                break;
            }
        }
        assert!(refused, "queue accepted 1 MiB against a 256 KiB cap");
        assert!(
            s.pty_write_backlog() <= marspot_term::frame_writer::cap::PTY,
            "backlog {} exceeded the cap",
            s.pty_write_backlog()
        );
    }

    /// reader thread fires one final wake so the main loop won't park
    /// forever waiting for more bytes.
    #[test]
    fn shell_exit_flips_is_exited() {
        let wake = Arc::new(AtomicBool::new(false));
        // Use `/bin/sh -c "exit 0"` so the child exits immediately.
        let pty_arc = Arc::new(
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
                ..Default::default()
            })
            .expect("spawn /bin/sh"),
        );
        let mut s = LocalSession {
            id: 1,
            pty: Arc::clone(&pty_arc),
            child_pid: 0,
            terminal: Terminal::new(80, 24),
            bytelog: None,
            rx: { let (_t, r) = mpsc::channel(); r },
            writer: spawn_pty_writer(0, Arc::clone(&pty_arc)),
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

