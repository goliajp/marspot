//! What the pane's output did, batch by batch, in the terms watches are
//! asked in: the output position, the latest command end and input
//! start, and which rows a position's output was written on.
//!
//! A row is numbered absolutely: the grid's scroll-push count plus the
//! row on screen, the same count a published frame's header carries.
//! A line keeps its number as it scrolls into the history.

mod ends;

use std::time::Instant;

use marspot_term::grid::{Grid, Reach};

use crate::SessionImpl;
use crate::pane_ctl::{Marks, local, shell_in_front};

use ends::CommandEnds;
pub(crate) use ends::{End, LatestEnds};

/// Points kept to find where a position's output went.
const RING: usize = 64;

/// A cell, numbered absolutely: (row, column).
pub(crate) type At = (u64, u16);

/// A position, and where the main screen's cursor was at it.
type Point = (u64, At);

const NOWHERE: Point = (0, (0, 0));

pub(crate) struct Track {
    ring: [Point; RING],
    /// Next slot to write, and how many are filled.
    head: usize,
    len: usize,
    /// The latest point taken on the main screen.
    main: Point,
    /// How far up and down the main screen's cursor went in the batch
    /// that went to the alternate screen, kept until it is left.
    held: Option<Reach>,
    pos: u64,
    last_output: Instant,
    ends: CommandEnds,
    inputs_seen: u64,
    /// Where the latest `OSC 133 ; B` the shell wrote in front was;
    /// none while the pane is taken at its quiet alone.
    input_start: Option<u64>,
    /// Whether such a mark came since the batch of the latest command
    /// end.
    input_since_end: bool,
    /// The terminal's generation at the last look: output a held grid
    /// kept is parsed when the hold ends, without the position moving.
    generation: u64,
}

/// What one batch of output did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Batch {
    /// Where the cursor was before the batch, and where it is.
    from: At,
    pub(crate) to: At,
    /// The rows it went up from and to in between, on the main screen.
    reach: Option<Reach>,
    /// Those of the batch that went to the alternate screen, on the
    /// batch that left it.
    before: Option<Reach>,
    /// The latest point on the main screen before the batch: `from`,
    /// or where the output that left the main screen started.
    main: Point,
}

impl Batch {
    /// The cell a watch from `since` is checked from.  What the batch
    /// that went to the alternate screen wrote before it was not read
    /// then; it is now, by a watch that is older than it.
    pub(crate) fn start(&self, since: u64) -> At {
        let (pos, at) = self.main;
        if since <= pos { at } else { self.from }
    }

    /// The rows the batch went up from and to, for a watch from
    /// `since`: with those of the batch that went to the alternate
    /// screen, for a watch older than it.
    pub(crate) fn reach(&self, since: u64) -> Option<Reach> {
        Reach::join(self.reach, self.before.filter(|_| since <= self.main.0))
    }
}

impl Track {
    pub(crate) fn new(session: &SessionImpl, resumed: Marks) -> Self {
        let inputs_seen = session.terminal().input_starts();
        let input_start = first_input_start(resumed.input_start, inputs_seen);
        let mut t = Self {
            ring: [NOWHERE; RING],
            head: 0,
            len: 0,
            main: NOWHERE,
            held: None,
            pos: local(session).output_pos(),
            last_output: Instant::now(),
            ends: CommandEnds::new(session.terminal(), resumed.ends),
            inputs_seen,
            input_start,
            input_since_end: input_start.is_some(),
            generation: session.terminal().generation(),
        };
        t.main = (t.pos, t.mark(session));
        t
    }

    pub(crate) fn pos(&self) -> u64 {
        self.pos
    }

    pub(crate) fn last_output(&self) -> Instant {
        self.last_output
    }

    /// Position and status of the latest `OSC 133 ; D`, of any, of
    /// a run's and of a prompt hook's.
    pub(crate) fn ends(&self) -> LatestEnds {
        self.ends.latest()
    }

    /// Position of the latest `OSC 133 ; B`, placed like a command end.
    pub(crate) fn input_start(&self) -> Option<u64> {
        self.input_start
    }

    /// Whether the shell may have drawn its prompt after `since`.  One
    /// that marks where input starts has only once it marked one after
    /// it: a `precmd` that runs something slow and silent is quiet long
    /// before the prompt comes.  One that never marked any is taken at
    /// its quiet alone.
    pub(crate) fn prompt_drawn(&self, since: u64) -> bool {
        self.input_start.is_none_or(|at| at > since)
    }

    /// Take the pane at its quiet alone again: a prompt watch held
    /// without the mark it waited for, so the shell no longer sets it.
    pub(crate) fn forget_input_start(&mut self) {
        self.input_start = None;
        self.input_since_end = false;
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
    pub(crate) fn after_batch(&mut self, session: &mut SessionImpl) -> Option<Batch> {
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
        let ended = self.ends.after_batch(session.terminal(), pos);
        let inputs = session.terminal().input_starts();
        // a mark from a program the shell runs (ssh) says nothing of
        // this shell's prompt; the foreground is read after the batch,
        // so one from a program that already ended (a short `cat` of a
        // file holding it) still counts, which a prompt watch's longer
        // quiet then corrects
        let input = inputs != self.inputs_seen && shell_in_front(session);
        self.inputs_seen = inputs;
        self.note_marks(pos, ended, input);
        let (from, main) = (self.last_at(), self.main);
        // the main screen's, kept while the alternate one is on: a
        // watch younger than the batch that entered it does not read
        // those rows
        let reach = session.terminal_mut().take_cursor_reach();
        let (reach, before) = if session.terminal().in_alt_screen() {
            self.held = Reach::join(self.held, reach);
            (None, None)
        } else {
            (reach, self.held.take())
        };
        let to = self.mark(session);
        Some(Batch { from, to, reach, before, main })
    }

    /// Keep the input start rule only while the shell marks an input
    /// start between command ends: one that stopped and still writes
    /// command ends is taken at its quiet again from its second command
    /// end on.  One that writes neither (`exec bash`) is left to the
    /// prompt watch's longer quiet.  A batch holds no order between its
    /// marks; a shell's hook writes the end before the next prompt's
    /// input start, so one in the same batch counts for the next end.
    fn note_marks(&mut self, pos: u64, ended: bool, input: bool) {
        if ended {
            if !self.input_since_end && !input {
                self.input_start = None;
            }
            self.input_since_end = false;
        }
        if input {
            self.input_start = Some(pos);
            self.input_since_end = true;
        }
    }

    /// Remember where the main screen's cursor is at the current
    /// position.
    pub(crate) fn mark(&mut self, session: &SessionImpl) -> At {
        let at = session.terminal().main_cursor();
        let pos = local(session).output_pos();
        if !session.terminal().in_alt_screen() {
            self.main = (pos, at);
        }
        let last = &mut self.ring[(self.head + RING - 1) % RING];
        if self.len > 0 && last.0 == pos {
            last.1 = at;
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

/// Where a new track puts the latest input start: the one the image
/// before handed over, or, for marks parsed before the track began, a
/// position older than any a watch can be given.
fn first_input_start(resumed: Option<u64>, inputs_seen: u64) -> Option<u64> {
    resumed.or((inputs_seen > 0).then_some(0))
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
            ring: [NOWHERE; RING],
            head: 0,
            len: 0,
            main: NOWHERE,
            held: None,
            pos: 0,
            last_output: Instant::now(),
            ends: CommandEnds::new(&marspot_term::terminal::Terminal::new(4, 2), LatestEnds::default()),
            inputs_seen: 0,
            input_start: None,
            input_since_end: false,
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

    #[test]
    fn a_track_that_starts_after_marks_puts_them_before_every_position() {
        assert_eq!(first_input_start(None, 0), None);
        assert_eq!(first_input_start(None, 3), Some(0));
        assert_eq!(first_input_start(Some(40), 3), Some(40));
        let mut t = track(&[]);
        t.input_start = first_input_start(None, 1);
        assert!(!t.prompt_drawn(0), "a mark at 0 is not after a watch from 0");
        assert!(!t.prompt_drawn(5));
    }

    #[test]
    fn a_prompt_counts_as_drawn_only_after_a_mark_past_the_watch() {
        let mut t = track(&[]);
        assert!(t.prompt_drawn(10), "no marks: taken at its quiet");
        t.note_marks(10, false, true);
        assert!(!t.prompt_drawn(10), "a mark at the watch's own position");
        assert!(t.prompt_drawn(9));
    }

    #[test]
    fn a_shell_that_stops_marking_input_starts_is_taken_at_its_quiet() {
        let mut t = track(&[]);
        // a hook writes the end and the next prompt's mark in one batch
        t.note_marks(10, true, true);
        t.note_marks(20, true, true);
        t.note_marks(30, true, false);
        assert_eq!(t.input_start, Some(20), "the mark at 20 came after the end at 10");
        t.note_marks(40, true, false);
        assert_eq!(t.input_start, None, "no mark between the ends at 30 and 40");
        t.note_marks(50, false, true);
        assert_eq!(t.input_start, Some(50), "marks again");
    }
}
