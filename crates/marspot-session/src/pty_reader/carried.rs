//! The reader's own carrying of a flood: the rates it takes before it lends the pty.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::lend::{KEPT_FOR, Lend, RATED_BYTES, rate};

/// Rates the reader takes before it offers the pty: the first is of a
/// flood getting under way.
const RATES_BEFORE_AN_OFFER: u32 = 2;

/// Rates the reader takes of a flood the loop carried slower before it
/// offers it again: what the flood is made of may have changed.  Twice
/// as many after each time the loop was slower again.  Asking has a
/// price, two megabytes carried the slower way and the pty changing
/// hands twice: a flood of slow-parsing text asked about every 32 MiB
/// went 2 % slower than one never lent (306 MB/s against 312), asked
/// once in 128 MiB it went as fast (310).
pub(super) const DECLINED_FOR: u32 = 32;

/// The reader's own carrying of a flood, a megabyte at a time.
pub(super) struct Carried {
    since: Option<Instant>,
    bytes: u64,
    /// Rates taken since the flood began or the pty came back.
    rates: u32,
    /// Rates taken since the loop gave the flood back as slower.
    declined_for: u32,
    /// Rates to take before a flood given back as slower is offered
    /// again.
    wait: u32,
}

impl Carried {
    pub(super) fn new() -> Self {
        Self { since: None, bytes: 0, rates: 0, declined_for: 0, wait: DECLINED_FOR }
    }

    /// The flood is over, or the pty was away: what was counted is not
    /// a megabyte of this reader's own any more.
    pub(super) fn restart(&mut self) {
        self.since = None;
        self.bytes = 0;
        self.rates = 0;
    }

    /// The flood has ended: the next one is offered afresh.
    pub(super) fn flood_over(&mut self, lend: &Lend) {
        #[cfg(test)]
        super::tests::note_flood_over();
        self.restart();
        self.declined_for = 0;
        self.wait = DECLINED_FOR;
        lend.declined.store(false, Ordering::SeqCst);
        lend.keep_for.store(KEPT_FOR, Ordering::Relaxed);
    }

    /// A chunk of `len` bytes of a flood that goes on has been queued.
    /// `true` when that completes a rate the pty may be offered on.
    pub(super) fn chunk(&mut self, len: usize, lend: &Lend) -> bool {
        let now = Instant::now();
        let Some(since) = self.since else {
            // from the end of the flood's first chunk: its beginning is
            // the program starting up
            self.since = Some(now);
            return false;
        };
        self.bytes += len as u64;
        if self.bytes < RATED_BYTES {
            return false;
        }
        lend.two_rate.store(rate(self.bytes, now.duration_since(since)), Ordering::SeqCst);
        self.since = Some(now);
        self.bytes = 0;
        self.rates += 1;
        if self.rates < RATES_BEFORE_AN_OFFER {
            return false;
        }
        if lend.declined.load(Ordering::SeqCst) {
            self.declined_for += 1;
            if self.declined_for < self.wait {
                return false;
            }
            self.wait = self.wait.saturating_mul(2);
            lend.declined.store(false, Ordering::SeqCst);
        }
        self.declined_for = 0;
        true
    }
}
