//! Whether a paste waits for the program to answer the return typed
//! before it.
//!
//! Answered is bracketed paste turned off since the return was queued
//! (zsh does that as it takes a line), or, for a program that keeps the
//! mode on, output that came after the return was written followed by
//! a quiet echo wait.  The echo of what was typed before the return can
//! still be on its way when the return goes out, so output alone, with
//! no quiet after it, is not an answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::ECHO_WAIT;

/// A paste is held at most this long after the return.  Held time
/// counts against the time its input is given to be read, so the
/// answer still comes within what the shell waits for it.
pub(super) const HOLD_AT_MOST: Duration = Duration::from_secs(1);

/// A return typed: queued at `at`, with the mode's off count then.
pub(super) struct Returned {
    at: Instant,
    offs: u32,
    /// Set by the PTY's writer thread once the return left its queue.
    written: Arc<AtomicBool>,
    /// From when the return was seen written: the output position
    /// then, and the latest position seen since with when it moved.
    after: Option<After>,
}

#[derive(Clone, Copy)]
struct After {
    pos: u64,
    seen: u64,
    moved_at: Instant,
}

impl Returned {
    /// `offs` is from before the return was queued; `pos` is the output
    /// position now.
    pub(super) fn new(at: Instant, offs: u32, written: Arc<AtomicBool>, pos: u64) -> Self {
        let mut r = Self { at, offs, written, after: None };
        r.observe(at, pos);
        r
    }

    /// Note the output position the moment the return is seen written.
    /// The writer thread wakes the loop then.
    pub(super) fn observe(&mut self, now: Instant, pos: u64) {
        if self.after.is_none() && self.written.load(Ordering::Acquire) {
            self.after = Some(After { pos, seen: pos, moved_at: now });
        }
    }

    /// The return is not seen written yet, and a paste may still wait
    /// for it: the loop has to look when it is.
    pub(super) fn pending(&self) -> bool {
        self.after.is_none() && self.at.elapsed() < HOLD_AT_MOST
    }

    /// When to look again while the paste waits; `None` once the
    /// program answered or the hold is over.  `pos` and `offs` are the
    /// terminal's now.
    pub(super) fn wait_until(&mut self, now: Instant, pos: u64, offs: u32) -> Option<Instant> {
        let over = self.at + HOLD_AT_MOST;
        if now >= over || offs != self.offs {
            return None;
        }
        self.observe(now, pos);
        // its being written wakes the loop
        let Some(after) = self.after.as_mut() else { return Some(over) };
        if pos != after.seen {
            (after.seen, after.moved_at) = (pos, now);
        }
        if after.seen == after.pos {
            return Some(over);
        }
        let quiet = (after.moved_at + ECHO_WAIT).max(self.at + ECHO_WAIT);
        (now < quiet).then_some(quiet.min(over))
    }
}
