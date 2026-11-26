//! What the forecast is computed from: the facts as the fact files
//! keep them, folded per pane and hour, bounded.
//!
//! Pruning drops whole hours older than `keep_from(now)`, the same
//! hour-aligned start a rebuild reads the files from, so a store fed
//! fact by fact and one fed every kept fact at once hold the same.

use std::collections::{BTreeMap, VecDeque};

use super::{HOUR, KEEP_S};
use crate::quota::Class;

/// Panes kept; past it the one seen longest ago is dropped.
pub const PANES_CAP: usize = 256;
/// Boundary rows kept per pane, newest.
pub const BOUNDARIES_CAP: usize = 1024;
/// Helper lifetimes kept per agent, and agents kept.
pub const HELPERS_CAP: usize = 256;
pub const AGENTS_CAP: usize = 16;
/// Hour rows kept: sixty days and a little.
pub const HOURS_CAP: usize = 60 * 24 + 48;

/// Index of a class in the per-class arrays: restricted first.
pub fn ix(c: Class) -> usize {
    match c {
        Class::Restricted => 0,
        Class::Plain => 1,
    }
}

/// The start of the facts kept as of `now`: whole hours, so a pruned
/// store and a rebuild read from here agree.
pub fn keep_from(now: i64) -> i64 {
    now.div_euclid(HOUR) * HOUR - KEEP_S
}

/// A pane's turn end, from its `boundary` fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub at: i64,
    /// Nothing held a move there (`holds=-`).
    pub clean: bool,
    /// Live background tasks, and the age of the oldest, seconds.
    pub tasks: u32,
    pub oldest_s: Option<u32>,
}

/// One hour of one pane: when its first fact in that hour came, and
/// what it used by class, weeks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneHour {
    pub first_at: i64,
    pub used: [f64; 2],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneFacts {
    pub hours: BTreeMap<i64, PaneHour>,
    pub last_at: i64,
    pub boundaries: VecDeque<Boundary>,
}

impl PaneFacts {
    /// When the pane's first kept fact came.
    pub fn first_at(&self) -> Option<i64> {
        self.hours.values().next().map(|h| h.first_at)
    }

    /// What it used of `class` in the hours starting in `[from, to)`.
    pub fn used(&self, class: usize, from: i64, to: i64) -> f64 {
        self.hours.range(from..to).map(|(_, h)| h.used[class]).sum()
    }
}

/// One fleet hour from the hourly roll-up: weeks by class, `None` for
/// an hour no shell saw.
pub type HourRow = (i64, Option<[f64; 2]>);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub panes: BTreeMap<u64, PaneFacts>,
    /// The fleet's hours, by start.
    pub hours: BTreeMap<i64, Option<[f64; 2]>>,
    /// Finished helpers' lifetimes by agent, seconds, oldest first.
    pub helpers: BTreeMap<String, VecDeque<u32>>,
}

impl Facts {
    /// Any fact of pane `sid` at `at`.
    pub fn seen(&mut self, at: i64, sid: u64) {
        self.pane(at, sid);
    }

    /// Pane `sid` used `weeks` of `class` at `at`.
    pub fn used(&mut self, at: i64, sid: u64, class: Class, weeks: f64) {
        self.pane(at, sid).used[ix(class)] += weeks;
    }

    pub fn boundary(&mut self, sid: u64, b: Boundary) {
        self.pane(b.at, sid);
        let p = self.panes.get_mut(&sid).expect("just added");
        if p.boundaries.len() == BOUNDARIES_CAP {
            p.boundaries.pop_front();
        }
        p.boundaries.push_back(b);
    }

    /// The hourly roll-up as the files hold it now; replaces what was set.
    pub fn set_hours(&mut self, rows: impl IntoIterator<Item = HourRow>) {
        self.hours = rows.into_iter().collect();
        while self.hours.len() > HOURS_CAP {
            self.hours.pop_first();
        }
    }

    /// Finished helpers' lifetimes as the helper log holds them now,
    /// `(agent, seconds)` oldest first; replaces what was set.
    pub fn set_helpers<'a>(&mut self, lives: impl IntoIterator<Item = (&'a str, u32)>) {
        self.helpers.clear();
        for (agent, secs) in lives {
            if !self.helpers.contains_key(agent) && self.helpers.len() == AGENTS_CAP {
                continue;
            }
            let v = self.helpers.entry(agent.to_string()).or_default();
            if v.len() == HELPERS_CAP {
                v.pop_front();
            }
            v.push_back(secs);
        }
    }

    /// Drop what the fact files no longer keep as of `now`.
    pub fn prune(&mut self, now: i64) {
        let from = keep_from(now);
        self.panes.retain(|_, p| {
            p.hours = p.hours.split_off(&from);
            while p.boundaries.front().is_some_and(|b| b.at < from) {
                p.boundaries.pop_front();
            }
            !p.hours.is_empty()
        });
    }

    fn pane(&mut self, at: i64, sid: u64) -> &mut PaneHour {
        if !self.panes.contains_key(&sid) && self.panes.len() == PANES_CAP {
            let stalest = self.panes.iter().min_by_key(|(s, p)| (p.last_at, **s)).map(|(s, _)| *s);
            if let Some(s) = stalest {
                self.panes.remove(&s);
            }
        }
        let p = self.panes.entry(sid).or_default();
        p.last_at = p.last_at.max(at);
        let h = at.div_euclid(HOUR) * HOUR;
        let ph = p.hours.entry(h).or_insert(PaneHour { first_at: at, used: [0.0; 2] });
        ph.first_at = ph.first_at.min(at);
        ph
    }
}
