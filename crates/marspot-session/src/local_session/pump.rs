//! Parsing what the reader has queued.

use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use super::{HOLD_CAP, LocalSession};

impl LocalSession {
    /// Drain every chunk the reader has queued, append each to the
    /// bytelog, feed it into the terminal parser.  Returns bytes
    /// consumed this tick (0 = nothing happened).
    pub fn pump(&mut self) -> usize {
        self.drain(None).0
    }

    /// [`Self::pump`], stopping once `budget` has been spent parsing.
    /// The flag says chunks are still queued.
    ///
    /// The reader queues whatever the PTY gives it, and a program can
    /// write faster than the parser reads -- `cat` of a big file, `yes`.
    /// Draining to empty then never ends while the program runs, and
    /// the loop that called it stops publishing frames, applying
    /// resizes and delivering keys, Ctrl-C included.
    pub fn pump_within(&mut self, budget: Duration) -> (usize, bool) {
        self.drain(Some(Instant::now() + budget))
    }

    fn drain(&mut self, deadline: Option<Instant>) -> (usize, bool) {
        let mut total = 0;
        let mut more = false;
        let raw = self.line.output_is_raw(self.pty.raw_master(), self.shell_pgrp());
        loop {
            if total > 0 && deadline.is_some_and(|d| Instant::now() >= d) {
                more = true;
                break;
            }
            match self.rx.try_recv() {
                Ok(chunk) => {
                    // see `line_discipline`: false unless a repair is pending
                    let converted = raw.then(|| crate::line_discipline::with_carriage_returns(chunk.bytes().to_vec()));
                    let bytes = converted.as_deref().unwrap_or(chunk.bytes());
                    // The bytelog is written whether or not the picture is
                    // held: everything that reads it must see the pane talk.
                    if let Some(b) = self.bytelog.as_mut() {
                        let _ = b.append(bytes);
                    }
                    total += bytes.len();
                    if self.hold {
                        self.held_bytes += bytes.len();
                        self.held.push(bytes.to_vec());
                        if self.held_bytes >= HOLD_CAP {
                            marspot_term::lx_warn!(
                                "l3.hold.cap_reached",
                                "held output hit the cap; releasing the freeze \
                                 mid-paint — the pane will visibly jump",
                                held_bytes = self.held_bytes as u64,
                                cap = HOLD_CAP as u64
                            );
                            self.hold_grid(false);
                        }
                    } else {
                        self.terminal.feed(bytes);
                    }
                    if let Some(r) = self.reader.as_ref() {
                        r.recycle(chunk);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if total > 0 {
            self.last_output = Some(Instant::now());
        }
        // The bytelog's size is what L1 reads to tell whether this pane
        // is talking; a burst must not wait in a buffer for 64 KiB of
        // company.  A flood fills buffers by itself, until it ends.
        if let Some(b) = self.bytelog.as_mut().filter(|_| !more && total < crate::publish_gate::FloodGate::BULK_BYTES) {
            b.hand_off();
        }
        self.flush_capability_responses();
        // see `reply_guard`: on every pump while replies are unaccounted
        // for -- any output, the shell's prompt included, or the loop's
        // short wake while watching
        if self.replies.is_watching() {
            let delivered = self.writer.backlog() == 0;
            self.replies.check_and_log(self.pty.raw_master(), self.shell_pgrp(), delivered, self.id);
        }
        // see `line_discipline`: a job starts drawing, or the prompt is back
        if total > 0 || self.line.wants_look() {
            self.line.look(self.pty.raw_master(), self.shell_pgrp(), self.id, self.writer.backlog() == 0);
        }
        (total, more)
    }
}
