//! Panes opened or closed through this socket a moment ago, laid over
//! the observation a list reads.  `pane.list` reads the observer's
//! snapshot, which can be up to its interval old: without this, a pane
//! just opened is missing from a list right after, and one just closed
//! is still in it.
//!
//! An open counts only over an observation older than it.  A close
//! counts over any: the pane's session can still be ending when a newer
//! observation is taken, and a session id is never used again.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a record is laid over observations; far past the
/// observer's interval.
const KEEP: Duration = Duration::from_secs(30);
/// Records kept at most; the oldest goes first.  Losing one only makes
/// a list as stale as the observation it reads.
const MAX: usize = 64;

/// A pane opened, as the list shows it until an observation has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Opened {
    pub sid: u64,
    pub cwd: String,
    /// The shadow pane's parent, 0 for a pane in a window.
    pub parent: u64,
}

enum Change {
    Opened(Opened),
    Closed(u64),
}

struct Rec {
    at: Instant,
    change: Change,
}

#[derive(Default)]
pub(super) struct Recent {
    recs: VecDeque<Rec>,
}

impl Recent {
    pub(super) fn opened(&mut self, opened: Opened, now: Instant) {
        self.push(Rec { at: now, change: Change::Opened(opened) }, now);
    }

    pub(super) fn closed(&mut self, sid: u64, now: Instant) {
        self.push(Rec { at: now, change: Change::Closed(sid) }, now);
    }

    fn push(&mut self, rec: Rec, now: Instant) {
        while self.recs.front().is_some_and(|r| now.saturating_duration_since(r.at) >= KEEP) {
            self.recs.pop_front();
        }
        if self.recs.len() == MAX {
            self.recs.pop_front();
        }
        self.recs.push_back(rec);
    }

    fn kept(&self, now: Instant) -> impl Iterator<Item = &Rec> {
        self.recs.iter().filter(move |r| now.saturating_duration_since(r.at) < KEEP)
    }

    /// Panes opened after an observation taken at `taken`, and not
    /// closed since, within the time records are kept.
    pub(super) fn opened_after(&self, taken: Instant, now: Instant) -> impl Iterator<Item = &Opened> {
        self.kept(now).filter(move |r| r.at > taken).filter_map(move |r| match &r.change {
            Change::Opened(o) if !self.closed_lately(o.sid, now) => Some(o),
            _ => None,
        })
    }

    /// Pane `sid` was closed within the time records are kept.
    pub(super) fn closed_lately(&self, sid: u64, now: Instant) -> bool {
        self.kept(now).any(|r| matches!(r.change, Change::Closed(s) if s == sid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(sid: u64) -> Opened {
        Opened { sid, cwd: "/w".into(), parent: 1 }
    }

    #[test]
    fn pane_control_recent_opens_count_only_over_older_views() {
        let t0 = Instant::now();
        let mut r = Recent::default();
        r.opened(opened(5), t0 + Duration::from_secs(1));
        let sids = |taken, now| r.opened_after(taken, now).map(|o| o.sid).collect::<Vec<_>>();
        assert_eq!(sids(t0, t0 + Duration::from_secs(2)), vec![5]);
        assert!(sids(t0 + Duration::from_secs(2), t0 + Duration::from_secs(2)).is_empty(), "the view has it");
        assert!(sids(t0, t0 + KEEP + Duration::from_secs(1)).is_empty(), "kept for a while only");
    }

    #[test]
    fn pane_control_recent_closes_count_over_any_view_for_a_while() {
        let t0 = Instant::now();
        let mut r = Recent::default();
        r.opened(opened(5), t0);
        r.closed(5, t0 + Duration::from_secs(1));
        assert!(r.closed_lately(5, t0 + Duration::from_secs(5)));
        assert!(!r.closed_lately(6, t0 + Duration::from_secs(5)));
        assert_eq!(r.opened_after(t0 - Duration::from_secs(1), t0 + Duration::from_secs(2)).count(), 0, "opened, then closed");
        assert!(!r.closed_lately(5, t0 + KEEP + Duration::from_secs(2)), "kept for a while only");
    }

    #[test]
    fn pane_control_recent_keeps_a_bounded_number() {
        let t0 = Instant::now();
        let mut r = Recent::default();
        for sid in 0..(MAX as u64 + 10) {
            r.opened(opened(sid), t0 + Duration::from_millis(sid + 1));
        }
        assert_eq!(r.recs.len(), MAX);
        assert_eq!(r.opened_after(t0, t0 + Duration::from_secs(1)).next().map(|o| o.sid), Some(10));
    }
}
