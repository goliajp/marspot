//! One shadow-affecting change at a time: a new pane, or a pane taken
//! out of its window into the shadows.  Each is checked against the
//! shadow tree and the count limits, asked of the core and answered;
//! two of them checked side by side could each pass and together make
//! two panes each other's parent, or go past a limit.  Nothing else
//! (input, reads, waits) passes through here.
//!
//! The checks read the observer's view, which can be up to two seconds
//! older than a change just made.  So every change that went through
//! the gate is kept here for a while and laid over any view taken
//! before it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, Reject};

use crate::observe::Observation;

/// How long a request waits for the one ahead of it; the holder's own
/// wait for the core is bounded (10 s for a new pane, 5 s for a move).
pub(super) const GATE_WITHIN: Duration = Duration::from_secs(12);
/// A change kept this long is in every view in use: a view is at most
/// two seconds old when a request starts, and the request waits at
/// most `GATE_WITHIN` here.
const KEEP_FOR: Duration = Duration::from_secs(30);
const KEEP_CAP: usize = 64;

#[derive(Clone, Copy)]
struct Change {
    at: Instant,
    sid: u64,
    /// The session's shadow parent after the change (0: in a window).
    parent: u64,
}

#[derive(Default)]
struct State {
    busy: bool,
    changes: VecDeque<Change>,
}

#[derive(Default)]
pub(super) struct Gate {
    state: Mutex<State>,
    freed: Condvar,
}

/// Held while one change is checked, asked for and recorded.
pub(super) struct Held<'g> {
    gate: &'g Gate,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait up to `within` for the change ahead to finish.
    pub(super) fn enter(&self, within: Duration) -> Result<Held<'_>, Reject> {
        let g = self.lock();
        let (mut g, waited) = self.freed.wait_timeout_while(g, within, |s| s.busy).unwrap_or_else(|p| p.into_inner());
        if waited.timed_out() && g.busy {
            return Err(Reject::new(ErrorCode::LimitRate, "another new pane or move into the shadows is still being made; try again")
                .retry_after(1_000));
        }
        g.busy = true;
        let now = Instant::now();
        while g.changes.front().is_some_and(|c| now.saturating_duration_since(c.at) > KEEP_FOR) {
            g.changes.pop_front();
        }
        Ok(Held { gate: self })
    }
}

impl Held<'_> {
    /// `obs` with every change made after it was taken laid over it.
    pub(super) fn tree(&self, obs: &Observation) -> Tree {
        let mut panes: HashMap<u64, (u64, bool)> = obs.sessions.iter().map(|s| (s.entry.id, (s.entry.shadow_of, s.live))).collect();
        for c in self.gate.lock().changes.iter().filter(|c| c.at > obs.at) {
            panes.insert(c.sid, (c.parent, true));
        }
        Tree { panes }
    }

    /// Session `sid` now has `parent` (0: a pane in a window), or may
    /// have: a request with no answer is counted as done.
    pub(super) fn record(&self, sid: u64, parent: u64) {
        let mut g = self.gate.lock();
        if g.changes.len() >= KEEP_CAP {
            g.changes.pop_front();
        }
        g.changes.push_back(Change { at: Instant::now(), sid, parent });
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.gate.lock().busy = false;
        self.gate.freed.notify_one();
    }
}

/// The sessions as the checks see them: `sid → (shadow parent, live)`.
pub(super) struct Tree {
    panes: HashMap<u64, (u64, bool)>,
}

impl Tree {
    pub(super) fn parent_of(&self, sid: u64) -> u64 {
        self.panes.get(&sid).map_or(0, |p| p.0)
    }

    pub(super) fn live(&self) -> usize {
        self.panes.values().filter(|p| p.1).count()
    }

    pub(super) fn shadows(&self) -> Vec<u64> {
        self.panes.iter().filter(|(_, p)| p.1 && p.0 != 0).map(|(sid, _)| *sid).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_gate_waits_are_bounded() {
        let gate = Gate::default();
        let held = gate.enter(GATE_WITHIN).unwrap();
        let t = Instant::now();
        let r = std::thread::scope(|s| s.spawn(|| gate.enter(Duration::from_millis(50)).map(|_| ())).join().unwrap());
        assert_eq!(r.unwrap_err().code, ErrorCode::LimitRate);
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(held);
        gate.enter(Duration::ZERO).unwrap();
    }
}
