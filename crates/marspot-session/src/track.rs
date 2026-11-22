//! What the pane's output did, batch by batch, in the terms watches are
//! asked in: the output position, the latest command end, and which
//! rows a position's output was written on.
//!
//! A row is numbered absolutely: the grid's scroll-push count plus the
//! row on screen, the same count a published frame's header carries.
//! A line keeps its number as it scrolls into the history.

use std::time::Instant;

use marspot_term::grid::Grid;

use crate::SessionImpl;
use crate::pane_ctl::local;

/// Pairs of (position, cursor) kept to find where a position's output
/// went.
const RING: usize = 64;

/// A cell, numbered absolutely: (row, column).
pub(crate) type At = (u64, u16);

pub(crate) struct Track {
    ring: [(u64, At); RING],
    /// Next slot to write, and how many are filled.
    head: usize,
    len: usize,
    pos: u64,
    last_output: Instant,
    commands_seen: u64,
    command_end: Option<(u64, Option<u8>)>,
    /// The terminal's generation at the last look: output a held grid
    /// kept is parsed when the hold ends, without the position moving.
    generation: u64,
}

/// What one batch of output did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Batch {
    /// Where the cursor was before the batch, and where it is.
    pub(crate) from: At,
    pub(crate) to: At,
}

impl Track {
    pub(crate) fn new(session: &SessionImpl, command_end: Option<(u64, Option<u8>)>) -> Self {
        let mut t = Self {
            ring: [(0, (0, 0)); RING],
            head: 0,
            len: 0,
            pos: local(session).output_pos(),
            last_output: Instant::now(),
            commands_seen: session.terminal().command_end().0,
            command_end,
            generation: session.terminal().generation(),
        };
        t.mark(session);
        t
    }

    pub(crate) fn pos(&self) -> u64 {
        self.pos
    }

    pub(crate) fn last_output(&self) -> Instant {
        self.last_output
    }

    /// Position and status of the latest `OSC 133 ; D`.
    pub(crate) fn command_end(&self) -> Option<(u64, Option<u8>)> {
        self.command_end
    }

    /// Note the turn's output, if there was any.
    ///
    /// A command end parsed in the batch is put at the position the
    /// batch ended at, not the one its bytes were at.  Every position a
    /// watch is given was read between two batches, so it is never
    /// inside one: the end lies after such a position exactly when the
    /// batch's last byte does, and comparing with `>` gives the same
    /// answer either way.
    ///
    /// A turn that only parsed output a held grid had kept is a batch
    /// too: the position does not move, the grid does.
    pub(crate) fn after_batch(&mut self, session: &SessionImpl) -> Option<Batch> {
        let pos = local(session).output_pos();
        let generation = session.terminal().generation();
        if pos == self.pos && generation == self.generation {
            return None;
        }
        self.generation = generation;
        if pos != self.pos {
            self.pos = pos;
            self.last_output = Instant::now();
        }
        // the batch's first end: a shell's hook may print its own right
        // behind the one a command printed, with another status
        let (count, latest) = session.terminal().command_end();
        if count != self.commands_seen {
            let first = session.terminal().command_end_status(self.commands_seen + 1).unwrap_or(latest);
            self.commands_seen = count;
            self.command_end = Some((pos, first));
        }
        let from = self.last_at();
        let to = self.mark(session);
        Some(Batch { from, to })
    }

    /// Remember where the cursor is at the current position.
    pub(crate) fn mark(&mut self, session: &SessionImpl) -> At {
        let at = cursor_at(session.terminal().grid());
        let pos = local(session).output_pos();
        if self.len > 0 && self.ring[(self.head + RING - 1) % RING].0 == pos {
            self.ring[(self.head + RING - 1) % RING].1 = at;
            return at;
        }
        self.ring[self.head] = (pos, at);
        self.head = (self.head + 1) % RING;
        self.len = (self.len + 1).min(RING);
        at
    }

    fn last_at(&self) -> At {
        self.ring[(self.head + RING - 1) % RING].1
    }

    /// The cell output after `since` started at: where the cursor was
    /// at the latest kept position not past it, or at the oldest kept
    /// one when `since` is older than all of them.
    pub(crate) fn at(&self, since: u64) -> At {
        let mut oldest = self.last_at();
        for i in 1..=self.len {
            let (pos, at) = self.ring[(self.head + RING - i) % RING];
            if pos <= since {
                return at;
            }
            oldest = at;
        }
        oldest
    }
}

/// The cursor, numbered absolutely.
pub(crate) fn cursor_at(grid: &Grid) -> At {
    let (col, row) = grid.cursor();
    (grid.scroll_push_count() + u64::from(row), col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(pairs: &[(u64, u64)]) -> Track {
        let mut t = Track {
            ring: [(0, (0, 0)); RING],
            head: 0,
            len: 0,
            pos: 0,
            last_output: Instant::now(),
            commands_seen: 0,
            command_end: None,
            generation: 0,
        };
        for &(p, r) in pairs {
            t.ring[t.head] = (p, (r, r as u16));
            t.head = (t.head + 1) % RING;
            t.len = (t.len + 1).min(RING);
        }
        t
    }

    #[test]
    fn a_watch_position_finds_the_row_its_output_started_on() {
        let t = track(&[(10, 1), (20, 3), (35, 9)]);
        assert_eq!(t.at(20), (3, 3));
        assert_eq!(t.at(34), (3, 3));
        assert_eq!(t.at(99), (9, 9));
        assert_eq!(t.at(5), (1, 1), "older than every pair: the oldest");
    }

    #[test]
    fn the_ring_keeps_the_latest_pairs() {
        let pairs: Vec<(u64, u64)> = (0..200).map(|i| (i * 10, i)).collect();
        let t = track(&pairs);
        assert_eq!(t.at(1990).0, 199);
        assert_eq!(t.at(0).0, 136, "the oldest of the last 64");
    }
}
