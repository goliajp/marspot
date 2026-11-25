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
//! before it.  A new pane the core did not answer for is kept too,
//! under a session id no session can have, so the counts stay on the
//! safe side until it expires.
//!
//! One window is left open: a move answered `unknown` after 5 s is
//! kept, but a core stuck longer than that can still carry the move
//! out later, after a newer view has made the kept change count for
//! nothing; the moved pane then sits in the shadows unchecked.  A new
//! pane the core creates only after its answer timed out is likewise
//! left uncounted once a newer view replaces the placeholder.

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
/// Full of changes still in use, the gate lets nothing through until
/// the oldest expires: dropping it could undercount a view in use.
const KEEP_CAP: usize = 64;
/// Session ids from here up stand for new panes the core did not
/// answer for; real ids count up from 1.
const UNANSWERED_SIDS: u64 = 1 << 63;

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
    unanswered: u64,
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
        let now = Instant::now();
        while g.changes.front().is_some_and(|c| now.saturating_duration_since(c.at) > KEEP_FOR) {
            g.changes.pop_front();
        }
        if let Some(oldest) = g.changes.front().filter(|_| g.changes.len() >= KEEP_CAP) {
            let left = KEEP_FOR.saturating_sub(now.saturating_duration_since(oldest.at));
            drop(g);
            self.freed.notify_one();
            return Err(Reject::new(ErrorCode::LimitRate, "too many new panes and moves into the shadows lately; try again")
                .retry_after(left.as_millis().max(1) as u64));
        }
        g.busy = true;
        Ok(Held { gate: self })
    }
}

impl Held<'_> {
    /// `obs` with every change made after it was taken laid over it.
    /// A change stamped with the view's own instant is laid over too:
    /// the clock is coarse enough that the two often read the same
    /// tick, and the view's registry read may then have missed it.
    pub(super) fn tree(&self, obs: &Observation) -> Tree {
        let mut panes: HashMap<u64, (u64, bool)> = obs.sessions.iter().map(|s| (s.entry.id, (s.entry.shadow_of, s.live))).collect();
        for c in self.gate.lock().changes.iter().filter(|c| c.at >= obs.at) {
            panes.insert(c.sid, (c.parent, true));
        }
        Tree { panes }
    }

    /// Session `sid` now has `parent` (0: a pane in a window), or may
    /// have: a request with no answer is counted as done.
    /// `enter` left room for this one.
    pub(super) fn record(&self, sid: u64, parent: u64) {
        self.gate.lock().changes.push_back(Change { at: Instant::now(), sid, parent });
    }

    /// A new pane under `parent` (0: in a window) the core did not
    /// answer for: it may exist.
    pub(super) fn record_unanswered(&self, parent: u64) {
        let mut g = self.gate.lock();
        let sid = UNANSWERED_SIDS | g.unanswered;
        g.unanswered = (g.unanswered + 1) % UNANSWERED_SIDS;
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

    fn view(at: Instant, sessions: &[(u64, u64)]) -> Observation {
        let entries = sessions
            .iter()
            .map(|&(id, shadow_of)| {
                let e = marspot_term::session_registry::SessionEntry {
                    id,
                    pid: 0,
                    socket: "/nonexistent".into(),
                    cols: 80,
                    rows: 24,
                    title: String::new(),
                    cwd: "/w".into(),
                    proto_version: 0,
                    created_at_unix: 0,
                    shm_name: String::new(),
                    shell_child_pid: 0,
                    build: String::new(),
                    stamps_regions: false,
                    shadow_of,
                };
                (e, true)
            })
            .collect();
        Observation::from_parts(at, marspot::pidtree::ProcTable::from_rows(Vec::new()), entries, |_| None)
    }

    #[test]
    fn shadow_gate_change_older_than_the_view_is_ignored() {
        let gate = Gate::default();
        let held = gate.enter(GATE_WITHIN).unwrap();
        let before = Instant::now();
        held.record(2, 1);
        std::thread::sleep(Duration::from_millis(5));
        // the newer view shows pane 2 back in a window: it wins
        let newer = held.tree(&view(Instant::now(), &[(1, 0), (2, 0)]));
        assert_eq!(newer.parent_of(2), 0);
        let older = held.tree(&view(before, &[(1, 0), (2, 0)]));
        assert_eq!(older.parent_of(2), 1);
    }

    #[test]
    fn shadow_gate_change_on_the_same_tick_as_the_view_counts() {
        let gate = Gate::default();
        let held = gate.enter(GATE_WITHIN).unwrap();
        held.record(2, 1);
        let at = gate.lock().changes.back().unwrap().at;
        assert_eq!(held.tree(&view(at, &[(1, 0), (2, 0)])).parent_of(2), 1);
    }

    #[test]
    fn shadow_gate_full_of_live_changes_refuses_and_keeps_the_oldest() {
        let gate = Gate::default();
        for sid in 0..KEEP_CAP as u64 {
            gate.enter(GATE_WITHIN).unwrap().record(100 + sid, 1);
        }
        let r = gate.enter(GATE_WITHIN).map(|_| ()).unwrap_err();
        assert_eq!(r.code, ErrorCode::LimitRate);
        assert!(r.retry_after_ms.is_some_and(|ms| ms > 25_000 && ms <= 30_000), "{r:?}");
        // the refusal left the gate free, and nothing was dropped
        assert!(!gate.lock().busy);
        assert_eq!(gate.lock().changes.front().map(|c| c.sid), Some(100));
        // once the oldest has expired there is room again
        gate.lock().changes.front_mut().unwrap().at = Instant::now().checked_sub(KEEP_FOR + Duration::from_secs(1)).unwrap();
        let held = gate.enter(Duration::ZERO).unwrap();
        assert_eq!(held.tree(&view(Instant::now().checked_sub(KEEP_FOR).unwrap(), &[])).parent_of(100), 0);
        assert_eq!(held.tree(&view(Instant::now().checked_sub(KEEP_FOR).unwrap(), &[])).parent_of(101), 1);
    }

    #[test]
    fn shadow_gate_unanswered_open_counts_until_expired() {
        let gate = Gate::default();
        let held = gate.enter(GATE_WITHIN).unwrap();
        let obs = view(Instant::now(), &[(1, 0)]);
        std::thread::sleep(Duration::from_millis(5));
        held.record_unanswered(1);
        held.record_unanswered(1);
        let tree = held.tree(&obs);
        assert_eq!(tree.live(), 3);
        let shadows = tree.shadows();
        assert_eq!(shadows.len(), 2);
        assert!(shadows.iter().all(|s| *s >= UNANSWERED_SIDS && tree.parent_of(*s) == 1), "{shadows:?}");
    }
}
