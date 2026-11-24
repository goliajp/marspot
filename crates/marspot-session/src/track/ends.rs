//! Command ends (`OSC 133 ; D`) by output position: the latest of any
//! kind, and the latest that a run's status line printed.

use marspot_term::terminal::Terminal;

/// Where an end was, and the status it said.
pub(crate) type End = (u64, Option<u8>);

/// The latest ends, as one image hands them to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LatestEnds {
    pub(crate) any: Option<End>,
    pub(crate) run: Option<End>,
}

pub(crate) struct CommandEnds {
    /// How many of each kind the terminal had parsed at the last look.
    seen: (u64, u64),
    latest: LatestEnds,
}

impl CommandEnds {
    pub(crate) fn new(term: &Terminal, resumed: LatestEnds) -> Self {
        Self { seen: (term.command_end().0, term.run_end().0), latest: resumed }
    }

    pub(crate) fn latest(&self) -> LatestEnds {
        self.latest
    }

    /// Note the ends of the batch that ended at `pos`: the first of
    /// each kind, as a shell's hook may print its own right behind the
    /// one a command printed, with another status.  Whether there was
    /// one.
    pub(crate) fn after_batch(&mut self, term: &Terminal, pos: u64) -> bool {
        let (count, last) = term.command_end();
        if count == self.seen.0 {
            return false;
        }
        self.latest.any = Some((pos, term.command_end_status(self.seen.0 + 1).unwrap_or(last)));
        self.seen.0 = count;
        let (runs, last) = term.run_end();
        if runs != self.seen.1 {
            self.latest.run = Some((pos, term.run_end_status(self.seen.1 + 1).unwrap_or(last)));
            self.seen.1 = runs;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runs_end_stays_the_latest_behind_other_ends() {
        let mut t = Terminal::new(20, 4);
        let mut ends = CommandEnds::new(&t, LatestEnds::default());
        assert!(!ends.after_batch(&t, 5));
        t.feed(b"\x1b]133;D;1\x07");
        assert!(ends.after_batch(&t, 10));
        assert_eq!(ends.latest(), LatestEnds { any: Some((10, Some(1))), run: None });
        // behind a hook's end and before more of them than are kept
        t.feed(b"\x1b]133;D;1\x07\x1b]133;D;7;marspot=run\x07");
        for _ in 0..10 {
            t.feed(b"\x1b]133;D;2\x07");
        }
        assert!(ends.after_batch(&t, 20));
        assert_eq!(ends.latest().run, Some((20, Some(7))));
        t.feed(b"\x1b]133;D;3\x07");
        assert!(ends.after_batch(&t, 30));
        assert_eq!(ends.latest(), LatestEnds { any: Some((30, Some(3))), run: Some((20, Some(7))) });
    }

    #[test]
    fn ends_start_from_the_ones_handed_over() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b]133;D;4;marspot=run\x07");
        let resumed = LatestEnds { any: Some((9, Some(2))), run: Some((8, Some(4))) };
        let mut ends = CommandEnds::new(&t, resumed);
        assert!(!ends.after_batch(&t, 12), "parsed before the track began");
        assert_eq!(ends.latest(), resumed);
    }
}
