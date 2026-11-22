//! The PTY's write half: chunks queued for a thread that may block.

use std::io;
use std::sync::Arc;

use marspot_term::pty::Pty;

use super::LocalSession;

/// Called on the writer thread once a chunk has left the queue.
pub(crate) type Written = Arc<dyn Fn() + Send + Sync>;

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
/// One chunk of bytes bound for the PTY.
///
/// The short-write retry loop lives here rather than in the queue: a
/// tty whose buffer is nearly full accepts a partial write, and the
/// original code discarded `write`'s return value entirely, silently
/// losing the tail of anything it didn't accept.
pub(super) struct PtyChunk {
    pub(super) pty: Arc<Pty>,
    pub(super) bytes: Vec<u8>,
    written: Option<Written>,
}

impl marspot_term::frame_writer::Queued for PtyChunk {
    fn queued_len(&self) -> usize {
        self.bytes.len()
    }
    fn write_all_to(&self, _sink: &mut dyn io::Write) -> io::Result<()> {
        // The destination is the PTY master this chunk carries, not the
        // sink — `Pty` is fd-shared rather than `Write`-shaped.
        let mut off = 0;
        while off < self.bytes.len() {
            match self.pty.write_shared(&self.bytes[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // the reader keeps the master non-blocking; a full tty
                // input queue is waited out here, on the writer thread
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut p = libc::pollfd { fd: self.pty.raw_master(), events: libc::POLLOUT, revents: 0 };
                    // SAFETY: one valid pollfd for the master this chunk's Pty owns.
                    unsafe { libc::poll(&mut p, 1, 100) };
                }
                // Dead fd (child gone).  Abandon this chunk; the session
                // tears down through the normal exit path.
                Err(_) => break,
            }
        }
        // a chunk abandoned on a dead fd was not written
        if off == self.bytes.len()
            && let Some(written) = &self.written
        {
            written();
        }
        Ok(())
    }
}

pub(super) type PtyWriter = marspot_term::frame_writer::BoundedWriter<PtyChunk>;

impl LocalSession {
    /// Queue `bytes` for the PTY.  Never blocks — see `PtyWriter`.
    ///
    /// `Ok(n)` means queued, not delivered; the only ordering promise
    /// is FIFO, which is all the callers (keystrokes, paste, injected
    /// input) need.  `WouldBlock` means the child has not read its
    /// stdin for long enough to fill the queue and the input was
    /// dropped — callers log it rather than retrying, since retrying
    /// into a full queue just drops again.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_then(bytes, None)
    }

    /// [`Self::write`], calling `written` once the last of `bytes` has
    /// left the queue (at once for no bytes).
    pub(crate) fn write_then(&mut self, bytes: &[u8], written: Option<Written>) -> io::Result<usize> {
        if bytes.is_empty() {
            if let Some(w) = written {
                w();
            }
            return Ok(0);
        }
        self.replies.wrote_other();
        let queued = self.enqueue(bytes, written);
        self.line.wrote_input(self.pty.raw_master(), self.shell_pgrp(), bytes);
        queued
    }

    pub(super) fn enqueue(&mut self, bytes: &[u8], written: Option<Written>) -> io::Result<usize> {
        // The cap check lives in `BoundedWriter::send` — duplicating it
        // here is how the two could drift apart.
        let chunk = PtyChunk { pty: Arc::clone(&self.pty), bytes: bytes.to_vec(), written };
        if !self.writer.send(chunk) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "pty write queue full ({} B pending, {} B dropped so far) \
                     or writer gone — the foreground process is not reading stdin",
                    self.writer.backlog(),
                    self.writer.dropped_bytes(),
                ),
            ));
        }
        Ok(bytes.len())
    }

    /// Bytes queued for the PTY but not yet accepted by it.  Non-zero
    /// for more than a moment means the foreground process is not
    /// reading its stdin.
    pub fn pty_write_backlog(&self) -> usize {
        self.writer.backlog()
    }
}
