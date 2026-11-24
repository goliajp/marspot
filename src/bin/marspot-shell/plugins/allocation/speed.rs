//! How fast a pane and an account use quota.
//!
//! A pane's speed is an exponential average with a three-hour half-life
//! over what it used in the last fifteen hours, by class.  It is a
//! function of those timestamped amounts and nothing else, so a shell
//! that restarts and reads the same fifteen hours back gets the same
//! speed it would have had running on.

use std::collections::VecDeque;

use super::{Class, RHO0};

/// Half-life of the pane speed, seconds.
pub(crate) const HALF_LIFE_S: f64 = 3.0 * 3600.0;

/// How far back a speed looks and a restart rebuilds from, seconds.
pub(crate) const WINDOW_S: i64 = 15 * 3600;

/// Amounts kept per pane: a minute apart at most, fifteen hours.
pub(crate) const PER_PANE_CAP: usize = 1024;

/// What one pane used, when.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PaneUse {
    /// `(unix seconds, class, weeks)`, oldest first.
    used: VecDeque<(i64, Class, f64)>,
    /// When it was first watched, clamped to the window.
    since: Option<i64>,
}

impl PaneUse {
    /// It was watched from `at`, whether or not it used anything.
    pub(crate) fn watched(&mut self, at: i64) {
        if self.since.is_none_or(|s| at < s) {
            self.since = Some(at);
        }
    }

    pub(crate) fn add(&mut self, at: i64, class: Class, amount: f64) {
        self.watched(at);
        if self.used.len() == PER_PANE_CAP {
            self.used.pop_front();
        }
        self.used.push_back((at, class, amount));
    }

    /// Drop what is older than the window as of `now`.
    pub(crate) fn prune(&mut self, now: i64) {
        let from = now - WINDOW_S;
        while self.used.front().is_some_and(|(at, _, _)| *at <= from) {
            self.used.pop_front();
        }
        self.since = self.since.map(|s| s.max(from));
    }

    /// Speed of `class` as of `now`, weeks an hour; `None` until it has
    /// been watched a half-life or has used something.
    pub(crate) fn rate(&self, now: i64, class: Class) -> Option<f64> {
        let from = now - WINDOW_S;
        let watched = self.since.is_some_and(|s| s.max(from) <= now - HALF_LIFE_S as i64);
        if !watched && self.used.is_empty() {
            return None;
        }
        let lambda = std::f64::consts::LN_2 / HALF_LIFE_S;
        let sum: f64 = self
            .used
            .iter()
            .filter(|(at, c, _)| *at > from && *at <= now && *c == class)
            .map(|(at, _, a)| a * (-lambda * (now - at) as f64).exp())
            .sum();
        // a steady speed over the whole window reads as itself
        let full = 1.0 - (-lambda * WINDOW_S as f64).exp();
        Some(sum * lambda * 3600.0 / full)
    }

    /// Both classes together, with `RHO0` when nothing is known.
    pub(crate) fn rate_or_default(&self, now: i64) -> (f64, f64, bool) {
        match (self.rate(now, Class::Restricted), self.rate(now, Class::Plain)) {
            (Some(f), Some(n)) => (f, n, true),
            _ => (0.0, RHO0, false),
        }
    }
}

/// An account's speed from its readings: the slope over a window,
/// started again after any drop.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Slope {
    /// `(unix seconds, reading)`, oldest first, none older than the
    /// window, none before the last drop.
    readings: VecDeque<(i64, f64)>,
}

impl Slope {
    const CAP: usize = 256;

    pub(crate) fn note(&mut self, at: i64, reading: f64, window_s: i64) {
        if self.readings.back().is_some_and(|(t, _)| *t >= at) {
            return;
        }
        if self.readings.back().is_some_and(|(_, r)| reading < *r) {
            self.readings.clear();
        }
        if self.readings.len() == Self::CAP {
            self.readings.pop_front();
        }
        self.readings.push_back((at, reading));
        while self.readings.front().is_some_and(|(t, _)| *t < at - window_s) {
            self.readings.pop_front();
        }
    }

    /// Weeks an hour, once the readings span ten minutes.
    pub(crate) fn rate(&self) -> Option<f64> {
        let (a, b) = (self.readings.front()?, self.readings.back()?);
        let span = (b.0 - a.0) as f64;
        (span >= 600.0).then(|| (b.1 - a.1) / (span / 3600.0))
    }
}
