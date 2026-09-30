//! Horizontal tab stops.
//!
//! A tab was a no-op here until now: the control was consumed and the
//! cursor did not move.  Anything that lays its output out with tabs —
//! `make`, `go`, a file indented with them, `ls -l` on some systems —
//! came out with its columns collapsed, and the kernel does not help:
//! a pseudo-terminal leaves `OXTABS` off, so the tab arrives verbatim
//! and expanding it is the terminal's job.
//!
//! Stops default to every eighth column, which is what every terminal
//! and every terminfo entry assumes, and programs may move them with
//! HTS, TBC, CHT and CBT.

/// Where the tabs are.
///
/// A bitset over columns.  One bit per column is small enough that a
/// wide window costs a few words, and it answers "is there a stop
/// here" without a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStops {
    bits: Vec<u64>,
    cols: u16,
}

/// The interval every terminal starts with.
pub const DEFAULT_INTERVAL: u16 = 8;

impl TabStops {
    /// Stops every eight columns, which is where they start.
    pub fn new(cols: u16) -> Self {
        let mut t = Self { bits: vec![0; words(cols)], cols };
        t.reset_defaults(0);
        t
    }

    /// Put default stops at every multiple of the interval from
    /// `from` upward, leaving anything below `from` alone.
    fn reset_defaults(&mut self, from: u16) {
        let mut c = from.div_ceil(DEFAULT_INTERVAL) * DEFAULT_INTERVAL;
        if c == 0 {
            // Column zero is never a stop: a tab typed at the left
            // margin has to go somewhere, and going nowhere is what
            // this module exists to stop.
            c = DEFAULT_INTERVAL;
        }
        while c < self.cols {
            self.set(c);
            c += DEFAULT_INTERVAL;
        }
    }

    /// Grow or shrink to `cols`.
    ///
    /// Stops that still fit are kept — a program that moved them did
    /// so on purpose and a resize is not a reason to forget — and the
    /// columns that just appeared get the default stops, which is what
    /// xterm does.
    pub fn resize(&mut self, cols: u16) {
        let old = self.cols;
        self.cols = cols;
        self.bits.resize(words(cols), 0);
        if cols > old {
            self.reset_defaults(old);
        } else {
            // Clear anything beyond the new width so a later grow does
            // not resurrect stops from a wider past.
            for c in cols..old {
                self.clear(c);
            }
        }
    }

    pub fn set(&mut self, col: u16) {
        if col < self.cols {
            self.bits[col as usize / 64] |= 1u64 << (col % 64);
        }
    }

    pub fn clear(&mut self, col: u16) {
        if (col as usize / 64) < self.bits.len() {
            self.bits[col as usize / 64] &= !(1u64 << (col % 64));
        }
    }

    pub fn clear_all(&mut self) {
        self.bits.iter_mut().for_each(|w| *w = 0);
    }

    pub fn is_stop(&self, col: u16) -> bool {
        (col as usize / 64) < self.bits.len()
            && self.bits[col as usize / 64] & (1u64 << (col % 64)) != 0
    }

    /// Where the cursor lands after `count` tabs from `col`.
    ///
    /// Past the last stop it goes to the final column and stays there.
    /// It does not wrap: a tab at the right margin is a no-op, which
    /// is what every terminal does and what programs that pad with
    /// tabs depend on.
    pub fn next(&self, col: u16, count: u16) -> u16 {
        let last = self.cols.saturating_sub(1);
        let mut c = col;
        for _ in 0..count.max(1) {
            match (c + 1..self.cols).find(|&x| self.is_stop(x)) {
                Some(stop) => c = stop,
                None => return last,
            }
        }
        c
    }

    /// Where the cursor lands after `count` backward tabs from `col`.
    /// Stops at column zero.
    pub fn prev(&self, col: u16, count: u16) -> u16 {
        let mut c = col;
        for _ in 0..count.max(1) {
            match (0..c).rev().find(|&x| self.is_stop(x)) {
                Some(stop) => c = stop,
                None => return 0,
            }
        }
        c
    }
}

fn words(cols: u16) -> usize {
    (cols as usize).div_ceil(64).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_start_every_eight_columns() {
        let t = TabStops::new(80);
        assert!(!t.is_stop(0), "column zero is not a stop");
        for c in [8u16, 16, 24, 72] {
            assert!(t.is_stop(c), "expected a stop at {c}");
        }
        for c in [1u16, 7, 9, 15] {
            assert!(!t.is_stop(c), "unexpected stop at {c}");
        }
    }

    #[test]
    fn a_tab_from_the_left_margin_goes_to_eight() {
        let t = TabStops::new(80);
        assert_eq!(t.next(0, 1), 8);
        assert_eq!(t.next(1, 1), 8);
        assert_eq!(t.next(7, 1), 8);
        assert_eq!(t.next(8, 1), 16);
    }

    #[test]
    fn several_tabs_at_once() {
        let t = TabStops::new(80);
        assert_eq!(t.next(0, 3), 24);
        // A count of zero means one, the way every CSI default does.
        assert_eq!(t.next(0, 0), 8);
    }

    #[test]
    fn past_the_last_stop_it_parks_at_the_right_margin() {
        let t = TabStops::new(20);
        // Stops at 8 and 16; a tab from 16 has nowhere to go.
        assert_eq!(t.next(16, 1), 19);
        assert_eq!(t.next(19, 1), 19, "and stays there rather than wrapping");
    }

    #[test]
    fn backward_tabs_stop_at_the_margin() {
        let t = TabStops::new(80);
        assert_eq!(t.prev(20, 1), 16);
        assert_eq!(t.prev(20, 2), 8);
        assert_eq!(t.prev(5, 1), 0);
        assert_eq!(t.prev(0, 1), 0);
    }

    #[test]
    fn a_program_can_move_them() {
        let mut t = TabStops::new(80);
        t.clear_all();
        assert_eq!(t.next(0, 1), 79, "with no stops a tab goes to the margin");
        t.set(3);
        t.set(40);
        assert_eq!(t.next(0, 1), 3);
        assert_eq!(t.next(3, 1), 40);
        t.clear(3);
        assert_eq!(t.next(0, 1), 40);
    }

    #[test]
    fn a_resize_keeps_what_a_program_set_and_defaults_the_rest() {
        let mut t = TabStops::new(20);
        t.clear_all();
        t.set(5);
        t.resize(40);
        assert!(t.is_stop(5), "a stop a program set survived the resize");
        assert!(t.is_stop(24), "the new columns got default stops");
        assert!(!t.is_stop(8), "and the old range was left as the program left it");
    }

    #[test]
    fn shrinking_forgets_the_columns_that_went_away() {
        let mut t = TabStops::new(80);
        assert!(t.is_stop(72));
        t.resize(20);
        t.resize(80);
        // 72 comes back as a default, not as a leftover — the point is
        // that nothing beyond the narrow width survived to be resurrected
        // in a different place.
        assert!(t.is_stop(72));
        assert!(t.is_stop(24));
    }

    #[test]
    fn a_one_column_terminal_does_not_panic() {
        let t = TabStops::new(1);
        assert_eq!(t.next(0, 1), 0);
        assert_eq!(t.prev(0, 1), 0);
    }
}
