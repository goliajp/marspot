//! How many rows a strip takes, and when that changes.  Each change is
//! a resize of the program in the pane, so there are few of them: the
//! height only grows while helpers are at work and goes in one step
//! once all have ended, and neither happens under the user's hands.

use super::compose::WINDOW;
use super::strip::MAX_ROWS;
use std::time::{Duration, Instant};

/// How long after helpers are gone the rows are given back, when the
/// shell has not said when they ended.
const KEEP: Duration = Duration::from_secs(3);
/// How long the user must not have typed.
const QUIET: Duration = Duration::from_secs(1);
/// And how long that is waited for.
const PUT_OFF: Duration = Duration::from_secs(30);
/// The blank row and `main`.
const HEAD: u16 = 2;

/// The rows for `n` helpers: five of them, and one that says how many
/// more there are.
pub(super) fn rows_for(n: usize) -> u16 {
    match n {
        0 => 0,
        n if n <= WINDOW => HEAD + n as u16,
        _ => MAX_ROWS,
    }
}

/// What is said of a pane's helpers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Said {
    /// Nothing yet.
    Unknown,
    /// It has none.
    Nothing,
    /// It has some, and the user cleared every one.
    Cleared,
    /// `need`: the rows for those whose sessions are ready, 0 while
    /// none was seen at work in this batch.
    Rows { works: bool, need: u16 },
}

/// What a height is decided by.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sight {
    pub said: Said,
    /// Claude draws on the main screen, where each resize leaves a copy
    /// of some rows in the history.
    pub inline: bool,
    /// Something else has the pane: nothing in it is to move.
    pub held: bool,
    /// When the user last typed in the pane.
    pub typed: Option<Instant>,
    pub now: Instant,
}

#[derive(Debug, Default)]
pub(super) struct Height {
    rows: u16,
    /// Taken all at once, for a program that is resized as little as
    /// can be.
    reserved: bool,
    /// Nothing is left to show in it, but giving it back costs a resize
    /// that the program is spared.
    spare: bool,
    /// Since when no helper is at work, and whether the shell still
    /// listed them then: it lets go of them in its own time.
    over: Option<(Instant, bool)>,
    /// Since when a change is owed.
    owed: Option<Instant>,
}

impl Height {
    /// What an earlier core left a session with.
    pub(super) fn found(rows: u16) -> Self {
        Self { rows, reserved: rows == MAX_ROWS, ..Self::default() }
    }

    /// Whether a strip can have left a session this many rows short.
    pub(super) fn could_be(rows: u16) -> bool {
        (rows_for(1)..=MAX_ROWS).contains(&rows)
    }

    pub(super) fn rows(&self) -> u16 {
        self.rows
    }

    /// The new height, if it changes now.
    pub(super) fn step(&mut self, s: &Sight) -> Option<u16> {
        self.note_over(s);
        let want = self.want(s);
        if want == self.rows {
            self.owed = None;
            return None;
        }
        let since = *self.owed.get_or_insert(s.now);
        if s.held || busy_until(s, since).is_some_and(|t| s.now < t) {
            return None;
        }
        self.reserved = want > self.rows && (self.reserved || s.inline);
        self.owed = None;
        self.rows = want;
        Some(want)
    }

    fn note_over(&mut self, s: &Sight) {
        self.over = match s.said {
            Said::Unknown | Said::Rows { works: true, .. } => None,
            Said::Rows { .. } => Some(self.over.map_or((s.now, true), |(t, _)| (t, true))),
            Said::Nothing | Said::Cleared => self.over.or(Some((s.now, false))),
        };
    }

    fn want(&mut self, s: &Sight) -> u16 {
        self.spare = false;
        match s.said {
            Said::Unknown | Said::Rows { need: 0, .. } => self.rows,
            Said::Rows { .. } if s.inline => MAX_ROWS,
            Said::Rows { need, .. } => self.rows.max(need),
            Said::Nothing | Said::Cleared if self.rows == 0 => 0,
            Said::Nothing | Said::Cleared if self.kept_until(s).is_some_and(|t| s.now < t) => self.rows,
            Said::Nothing | Said::Cleared if self.reserved && s.inline => {
                self.spare = true;
                self.rows
            }
            Said::Nothing | Said::Cleared => 0,
        }
    }

    fn kept_until(&self, s: &Sight) -> Option<Instant> {
        self.over.map(|(t, listed)| if listed && s.said == Said::Nothing { t } else { t + KEEP })
    }

    /// When to look again, if nothing happens before.
    pub(super) fn due(&self, s: &Sight) -> Option<Instant> {
        match self.owed {
            Some(_) if s.held => None,
            Some(since) => busy_until(s, since),
            None if self.rows > 0 && matches!(s.said, Said::Nothing | Said::Cleared) && !self.spare => self.kept_until(s),
            None => None,
        }
    }

    /// The program is resized anyway: rows that were spare go with it.
    /// Whether they did.
    pub(super) fn resized(&mut self) -> bool {
        let goes = self.spare;
        if goes {
            *self = Self::default();
        }
        goes
    }
}

fn busy_until(s: &Sight, since: Instant) -> Option<Instant> {
    s.typed.map(|t| (t + QUIET).min(since + PUT_OFF))
}

#[cfg(test)]
#[path = "height_tests.rs"]
mod tests;
