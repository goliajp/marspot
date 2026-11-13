//! One reader's place among the panes reading bulk output.

use std::time::{Duration, Instant};

use super::lease::{Lease, Leases};

/// How much of a burst is read before the reader queues for a turn.
const BURST_BYTES: usize = 32 * 1024;

/// How long a reader keeps its turn at bulk output while others may be
/// waiting for one.  Nine panes flooding get a slice each every 9 ms.
const TURN: Duration = Duration::from_millis(2);

pub(super) struct Turn {
    leases: Leases,
    held: Option<(Lease, Instant)>,
    /// A turn of this flood was used up and passed on: the next one is
    /// waited for before reading on.
    flooding: bool,
    /// Some other pane held a lease this flood wanted.
    crowded: bool,
}

impl Turn {
    pub(super) fn new(leases: Leases) -> Self {
        Self { leases, held: None, flooding: false, crowded: false }
    }

    /// In bulk output that has not paused since it took a turn.
    pub(super) fn in_flood(&self) -> bool {
        self.held.is_some() || self.flooding
    }

    /// Called with `len` bytes of bulk output in hand.  The start of a
    /// burst takes a turn only if one is free -- a screenful repainted
    /// while other panes flood does not queue behind them -- and
    /// output that keeps coming waits for one, until `leave` is true:
    /// see [`super::lease::hurry`].  `true` when a turn came out of a
    /// queue, which may have taken a while.
    pub(super) fn enter(&mut self, len: usize, leave: impl Fn() -> bool) -> bool {
        if self.held.is_some() {
            return false;
        }
        let queue = self.flooding || len >= BURST_BYTES;
        let mut lease = self.leases.try_take();
        if lease.is_none() {
            self.crowded = true;
            if queue {
                lease = self.leases.take(leave);
            }
        }
        self.held = lease.map(|lease| (lease, Instant::now()));
        queue && self.held.is_some()
    }

    /// The output stopped.
    pub(super) fn end(&mut self) {
        self.held = None;
        self.flooding = false;
        self.crowded = false;
    }

    /// No other pane has been in the way of this flood.
    pub(super) fn alone(&self) -> bool {
        !self.crowded
    }

    /// Give the turn to whoever waits; this flood is not over.
    pub(super) fn pass(&mut self) {
        if self.held.take().is_some() {
            self.flooding = true;
        }
    }

    pub(super) fn pass_if_spent(&mut self) {
        if self.held.as_ref().is_some_and(|(_, since)| since.elapsed() >= TURN) {
            self.pass();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::lease;
    use super::*;

    const KILOBYTE: usize = 1024;

    /// A flood asks again after a short read only while no other pane
    /// has been in its way, and finds that out afresh with each flood.
    #[test]
    fn a_flood_is_alone_until_it_finds_a_lease_taken() {
        let path = lease::tests::lock_file("alone");
        let mut free = Turn::new(Leases::at(&path));
        free.enter(KILOBYTE, || false);
        assert!(free.alone(), "every lease was there to take");
        drop(free);

        let other = lease::tests::holder(&path);
        let mut turn = Turn::new(Leases::at(&path));
        turn.enter(KILOBYTE, || false);
        assert!(!turn.alone(), "another pane holds the leases");
        lease::tests::release(other);
        turn.enter(KILOBYTE, || false);
        assert!(turn.held.is_some());
        assert!(!turn.alone(), "for as long as this flood lasts");
        turn.end();
        assert!(turn.alone());
        let _ = std::fs::remove_file(path);
    }
}
