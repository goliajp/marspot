//! How many rows a strip takes, and when that changes.  Each change is
//! a resize of the program in the pane, so there are few of them: the
//! height is that of the rows drawn, goes in one step once all helpers
//! have ended, and changes neither under the user's hands nor while the
//! agent's own list comes and goes.  On the main screen, where a resize
//! leaves rows twice in the history, it only grows.

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
/// The blank row and `main`, when the strip draws its own.
const HEAD: u16 = 2;
/// How long the agent's own list must have been there before the
/// height follows, and how long gone: its own prompts and menus hide it
/// for a while, and each change is two resizes.
const APPEARS: Duration = Duration::from_secs(1);
const GOES: Duration = Duration::from_secs(3);

fn steady(on: bool) -> Duration {
    if on { APPEARS } else { GOES }
}

/// The rows for `n` helpers: five of them and one that says how many
/// more there are, after the agent's own list (`merged`) or a head of
/// their own.  Under them nothing, as under the agent's own list on the
/// alternate screen; on the main screen (`inline`) one blank row.
pub(super) fn rows_for(n: usize, merged: bool, inline: bool) -> u16 {
    if n == 0 {
        return 0;
    }
    let head = if merged { 0 } else { HEAD };
    let listed = n.min(WINDOW) as u16 + u16::from(n > WINDOW);
    (head + listed + u16::from(inline)).min(MAX_ROWS)
}

/// Whether the agent draws its own list, as the height takes it: what
/// the screen showed once it has shown it for [`APPEARS`], or not
/// shown it for [`GOES`].
#[derive(Debug, Default)]
pub(super) struct Merge {
    merged: bool,
    /// What the screen shows, and since when; None: never looked at.
    seen: Option<(bool, Instant)>,
}

impl Merge {
    /// The screen shows the agent's list, or not.  The first look is
    /// taken as it is: nothing was drawn for it yet.
    pub(super) fn saw(&mut self, on: bool, now: Instant) {
        match self.seen {
            None => {
                self.merged = on;
                self.seen = Some((on, now));
            }
            Some((was, _)) if was != on => self.seen = Some((on, now)),
            Some(_) => {}
        }
    }

    /// Bring `merged` up to `now`.
    pub(super) fn settle(&mut self, now: Instant) {
        if let Some((on, since)) = self.seen
            && now >= since + steady(on)
        {
            self.merged = on;
        }
    }

    pub(super) fn merged(&self) -> bool {
        self.merged
    }

    /// When what is seen is to be taken.
    pub(super) fn due(&self) -> Option<Instant> {
        self.seen.filter(|(on, _)| *on != self.merged).map(|(on, since)| since + steady(on))
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
    /// every one of this batch had ended when it was first heard of.
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
        (rows_for(1, true, false)..=MAX_ROWS).contains(&rows)
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
            Said::Rows { need, .. } if s.inline => self.rows.max(need),
            Said::Rows { need, .. } => need,
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
