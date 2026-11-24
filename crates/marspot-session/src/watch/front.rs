//! When the pane's shell was back in front, as one watch saw it
//! (`prompt`, or a run's end).  The shell takes the terminal back before it draws its prompt
//! and may be silent in between, so the quiet such a watch asks for
//! is measured from then, not from what a command printed before.

use std::time::Instant;

#[derive(Default)]
pub(super) struct Front {
    /// Something else was in front at the last look.
    away: bool,
    back: Option<Instant>,
}

impl Front {
    /// The shell's processes changed, or the watch begins.
    pub(super) fn seen(&mut self, shell: bool, now: Instant) {
        if !shell {
            self.away = true;
        } else if std::mem::take(&mut self.away) {
            self.back = Some(now);
        }
    }

    /// The shell is in front as the quiet ends.  Whether it is news: the
    /// last look may have come between a command's exit and the shell
    /// taking the terminal back, which no event follows.  Output since
    /// that look may be the command's as well as the shell's, so it does
    /// not say when the shell was back; the quiet starts again, once.
    /// Open: a command that came and went between two looks was never
    /// seen in front, and the quiet is not moved for it.
    pub(super) fn back_unseen(&mut self, now: Instant) -> bool {
        let unseen = std::mem::take(&mut self.away);
        if unseen {
            self.back = Some(now);
        }
        unseen
    }

    /// The quiet is over: whether the shell is in front, and was when
    /// the quiet began.  `parked` when it is not in front: nothing is
    /// due until the output or the process group changes.
    pub(super) fn settled(&mut self, parked: &mut bool, shell: bool, now: Instant) -> bool {
        *parked = !shell;
        if *parked {
            self.seen(false, now);
        }
        !*parked && !self.back_unseen(now)
    }

    /// Where the quiet starts.
    pub(super) fn quiet_from(&self, last_output: Instant) -> Instant {
        self.back.map_or(last_output, |back| back.max(last_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn watch_quiet_starts_at_the_later_of_the_output_and_the_shell_back_in_front() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut f = Front::default();
        f.seen(true, at(1));
        assert_eq!(f.quiet_from(t0), t0, "in front all along");
        f.seen(false, at(10));
        f.seen(true, at(1500));
        assert_eq!(f.quiet_from(t0), at(1500));
        f.seen(true, at(1600));
        assert_eq!(f.quiet_from(t0), at(1500), "still in front is not back again");
        assert_eq!(f.quiet_from(at(1700)), at(1700));
    }

    #[test]
    fn watch_a_shell_found_back_at_the_end_of_the_quiet_starts_it_again_once() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut f = Front::default();
        f.seen(false, at(900));
        assert!(f.back_unseen(at(1000)));
        assert_eq!(f.quiet_from(t0), at(1000));
        assert!(!f.back_unseen(at(2000)));
        f.seen(false, at(3000));
        assert!(f.back_unseen(at(4100)));
        assert_eq!(f.quiet_from(at(3100)), at(4100), "what was printed at 3100 may be the command's");
        assert!(!f.back_unseen(at(5100)));
    }
}
