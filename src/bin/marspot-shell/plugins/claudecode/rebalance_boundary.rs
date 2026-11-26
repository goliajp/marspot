//! One line per rest of a pane that has a standing target: moved, or
//! what held it.
//!
//! A rest is one stretch in which the pane state machine has the pane
//! waiting for the user.  The waiting line says what held a pane when
//! its target or the reason changed; it cannot say what held it through
//! a given rest, and a rest is when a resting move can happen.  On
//! 2026-11-26 a pane rested 68 times for 90 s or longer with a move
//! standing and was never moved, and the log could not say why.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::rebalance::Veto;

/// A rest whose start moved by more than this is another rest.
const SAME_REST: Duration = Duration::from_secs(2);
/// Changes kept per rest; past that only the last is overwritten.
const SEEN_CAP: usize = 8;

/// Where the core last said a pane should go, and the rule's word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub from: u8,
    pub to: u8,
    pub word: &'static str,
}

/// What one look at a resting pane found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Held(Veto),
    WouldMove,
    Moved,
}

impl Outcome {
    fn key(self) -> &'static str {
        match self {
            Outcome::Held(v) => v.key(),
            Outcome::WouldMove => "would_move",
            Outcome::Moved => "moved",
        }
    }
}

struct Rest {
    started: Instant,
    last_at: Instant,
    target: Target,
    /// Each change of outcome, with the seconds into the rest it came.
    seen: Vec<(Outcome, u64)>,
}

impl Rest {
    fn note(&mut self, now: Instant, o: Outcome) {
        self.last_at = now;
        let at = now.saturating_duration_since(self.started).as_secs();
        let full = self.seen.len() >= SEEN_CAP;
        match self.seen.last_mut() {
            Some((last, _)) if *last == o => {}
            Some(last) if full => *last = (o, at),
            _ => self.seen.push((o, at)),
        }
    }

    fn line(&self, sid: u64, gone: bool) -> String {
        let t = self.target;
        let last = self.seen.last().map(|(o, _)| *o);
        let outcome = match last {
            Some(Outcome::Moved) => "moved".to_string(),
            Some(Outcome::WouldMove) => "would_move".to_string(),
            Some(Outcome::Held(v)) => format!("held why={}", v.key()),
            None => "held why=unseen".to_string(),
        };
        let seen: Vec<String> = self.seen.iter().map(|(o, at)| format!("{}@{at}", o.key())).collect();
        let rest_s = self.last_at.saturating_duration_since(self.started).as_secs();
        format!(
            "sid={sid} P{} → P{} ({}) rest_s={rest_s} outcome={outcome} seen={}{}",
            t.from,
            t.to,
            t.word,
            seen.join(","),
            if gone { " ended=gone" } else { "" }
        )
    }
}

/// The standing targets and the rests being watched, one of each per
/// live pane at most.
#[derive(Default)]
pub(super) struct Boundaries {
    standing: HashMap<u64, Target>,
    open: HashMap<u64, Rest>,
    /// The start of the rest a move closed: the sweeps until the scan
    /// binds the new claude still see that rest, and it has its line.
    closed: HashMap<u64, Instant>,
}

impl Boundaries {
    /// The core decided a move for the pane.
    pub(super) fn stand(&mut self, sid: u64, t: Target) {
        self.standing.insert(sid, t);
    }

    /// The core, with the pane resting, decided no move.
    pub(super) fn drop_target(&mut self, sid: u64) {
        self.standing.remove(&sid);
    }

    /// The pane's standing target while it is still on the account the
    /// target was decided from.
    pub(super) fn standing(&mut self, sid: u64, from: u8) -> Option<Target> {
        if self.standing.get(&sid).is_some_and(|t| t.from != from) {
            self.standing.remove(&sid);
        }
        self.standing.get(&sid).copied()
    }

    /// One look at a pane: `resting_for` how long the state machine has
    /// had it waiting for the user, when it has.  Hands back the lines
    /// of the rests this look ended.
    pub(super) fn look(&mut self, sid: u64, now: Instant, resting_for: Option<Duration>, target: Option<Target>, o: Outcome) -> Vec<String> {
        let mut lines = Vec::new();
        let started = resting_for.map(|d| now.checked_sub(d).unwrap_or(now));
        let same = |r: &Rest| started.is_some_and(|s| abs_diff(s, r.started) <= SAME_REST);
        if self.open.get(&sid).is_some_and(|r| !same(r))
            && let Some(r) = self.open.remove(&sid)
        {
            lines.push(r.line(sid, false));
        }
        let Some(started) = started else { return lines };
        if self.closed.get(&sid).is_some_and(|c| abs_diff(started, *c) <= SAME_REST) {
            return lines;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = self.open.entry(sid) {
            let Some(target) = target else { return lines };
            e.insert(Rest { started, last_at: now, target, seen: Vec::new() });
        }
        let Some(r) = self.open.get_mut(&sid) else { return lines };
        let o = match target {
            Some(t) => {
                r.target = t;
                o
            }
            None => Outcome::Held(Veto::CoreStays),
        };
        r.note(now, o);
        if o == Outcome::Moved
            && let Some(r) = self.open.remove(&sid)
        {
            self.closed.insert(sid, r.started);
            lines.push(r.line(sid, false));
        }
        lines
    }

    /// Forget panes that are gone; a rest they were in ends with them.
    pub(super) fn retain(&mut self, alive: impl Fn(u64) -> bool) -> Vec<String> {
        self.standing.retain(|sid, _| alive(*sid));
        self.closed.retain(|sid, _| alive(*sid));
        let gone: Vec<u64> = self.open.keys().copied().filter(|sid| !alive(*sid)).collect();
        gone.into_iter().filter_map(|sid| self.open.remove(&sid).map(|r| r.line(sid, true))).collect()
    }
}

fn abs_diff(a: Instant, b: Instant) -> Duration {
    if a > b { a - b } else { b - a }
}

#[cfg(test)]
#[path = "rebalance_boundary_tests.rs"]
mod tests;
