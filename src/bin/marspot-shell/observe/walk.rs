//! The observer's thread: when it walks, and what it tells the
//! supervisor.
//!
//! The supervisor no longer passes once a second while a pane is awake,
//! and its passes were what kept the observer to its interval.  So while
//! the supervisor is awake the observer keeps to it on its own, and
//! knocks the supervisor when a walk finds the panes changed in a way a
//! pass acts on: a pane opened or closed, its shell replaced, another
//! process group in front of its terminal or another program leading
//! it, or a kimi started or gone anywhere on the machine (the kimi
//! plugin keeps track of every kimi, in a pane or not).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use super::{OBSERVER, Observation, Shared};

/// Walks that found the panes changed (`shape`).
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// How many walks have found the panes changed: a pass compares it with
/// the count it saw last.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::Acquire)
}

/// While on, the observer keeps to its interval whether or not anybody
/// reads; off, it goes back to waiting for a reader.  Does nothing before
/// the observer is started.
pub fn keep_walking(on: bool) {
    if let Some(o) = OBSERVER.get() {
        let mut g = o.shared.state();
        if g.awake != on {
            g.awake = on;
            drop(g);
            o.shared.changed.notify_all();
        }
    }
}

/// What a pass acts on in an observation, as one number.
pub(super) fn shape(obs: &Observation) -> u64 {
    let mut h = DefaultHasher::new();
    for s in &obs.sessions {
        (s.entry.id, s.live, s.shell.map(|id| (id.pid, id.start_us)), s.front_pgid).hash(&mut h);
        if let Some(p) = s.front() {
            (p.row.pid, p.row.start_us, p.name()).hash(&mut h);
        }
    }
    for row in obs.table.rows().iter().filter(|r| crate::plugins::kimi::looks_like_kimi(r)) {
        (row.pid, row.start_us).hash(&mut h);
    }
    h.finish()
}

pub(super) fn run(shared: &Shared, mut build: impl FnMut(&AtomicU64) -> Observation, interval: Duration) {
    let mut seen = shape(&shared.state().latest);
    loop {
        {
            let mut g = shared.state();
            loop {
                if g.stop {
                    return;
                }
                if g.wanted {
                    break;
                }
                if !g.read && !g.awake {
                    g = shared.changed.wait(g).unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
                let due = g.latest.at + interval;
                let now = Instant::now();
                if now >= due {
                    break;
                }
                g = shared.changed.wait_timeout(g, due - now).unwrap_or_else(PoisonError::into_inner).0;
            }
            g.wanted = false;
            g.read = false;
        }
        let obs = Arc::new(build(&shared.walks));
        let shaped = shape(&obs);
        let mut g = shared.state();
        if g.stop {
            return;
        }
        let old = std::mem::replace(&mut g.latest, obs);
        drop(g);
        shared.changed.notify_all();
        drop(old);
        if std::mem::replace(&mut seen, shaped) != shaped {
            CHANGES.fetch_add(1, Ordering::Release);
            crate::pump::knock();
        }
    }
}
