//! What the account and reclaim rules need to know about helpers: which
//! panes are helpers (shadow panes), which have helpers working for
//! them, and the account moves made for a helper, for the helpers'
//! own record.
//!
//! A pane that handed work to a helper stops and waits for the result,
//! which is exactly what reads as resting; a helper waits between two
//! messages the same way.  Reclaiming either, or moving it to another
//! account while it rests, restarts the agent for nothing: nobody comes
//! back to wake a helper, and the restart throws away the prompt cache.

pub(in crate::plugins) mod turns;

use std::collections::VecDeque;
use std::sync::Mutex;

/// The live sessions that are helpers, and those with live helpers.
/// Rebuilt from the registry on every scan, so it holds no more than
/// the sessions do.
#[derive(Debug, Default, Clone)]
pub(in crate::plugins) struct HelperRoles {
    shadows: Vec<u64>,
    hosts: Vec<u64>,
}

impl HelperRoles {
    /// From each live session's `(id, shadow_of)`.
    pub(in crate::plugins) fn of(live: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let mut roles = Self::default();
        for (sid, parent) in live {
            if parent != 0 {
                roles.shadows.push(sid);
                if !roles.hosts.contains(&parent) {
                    roles.hosts.push(parent);
                }
            }
        }
        roles
    }

    pub(in crate::plugins) fn of_observation(obs: &crate::observe::Observation) -> Self {
        Self::of(obs.live().map(|s| (s.entry.id, s.entry.shadow_of)))
    }

    pub(in crate::plugins) fn is_helper(&self, sid: u64) -> bool {
        self.shadows.contains(&sid)
    }

    pub(in crate::plugins) fn has_helpers(&self, sid: u64) -> bool {
        self.hosts.contains(&sid)
    }

    /// Left out of idle reclaim and of moves made only because it rests.
    pub(in crate::plugins) fn sits_out(&self, sid: u64) -> bool {
        self.is_helper(sid) || self.has_helpers(sid)
    }
}

/// A helper's agent was moved to another account after it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::plugins) struct AccountChanged {
    pub sid: u64,
    /// `claude` or `kimi`.
    pub agent: &'static str,
    pub from: u8,
    pub to: u8,
    /// Unix seconds.
    pub at: i64,
}

/// Kept until drained; the oldest goes first past this many.
const CHANGES_CAP: usize = 64;

type Waker = std::sync::Arc<dyn Fn() + Send + Sync>;

struct Changes {
    queue: VecDeque<AccountChanged>,
    wake: Option<Waker>,
}

// process-global: its tests rely on nextest running each test in its own process
static CHANGES: Mutex<Changes> = Mutex::new(Changes { queue: VecDeque::new(), wake: None });

/// Called after each change is queued, so its reader need not look for
/// changes on a timer.  Replaces any earlier one.
pub(in crate::plugins) fn on_account_changed(wake: Waker) {
    CHANGES.lock().unwrap_or_else(|e| e.into_inner()).wake = Some(wake);
}

pub(in crate::plugins) fn account_changed(change: AccountChanged) {
    let mut c = CHANGES.lock().unwrap_or_else(|e| e.into_inner());
    if c.queue.len() == CHANGES_CAP {
        c.queue.pop_front();
    }
    c.queue.push_back(change);
    // called unlocked: the waker may well drain the queue itself
    let wake = c.wake.clone();
    drop(c);
    if let Some(wake) = wake {
        wake();
    }
}

/// Every change queued since the last call, oldest first.
pub(in crate::plugins) fn drain_account_changes() -> Vec<AccountChanged> {
    CHANGES.lock().unwrap_or_else(|e| e.into_inner()).queue.drain(..).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_and_the_panes_they_work_for_sit_out() {
        // 5 and 6 work for 2; 7 works for 5; 9 names a parent that is gone
        let roles = HelperRoles::of([(1, 0), (2, 0), (5, 2), (6, 2), (7, 5), (9, 40)]);
        assert!(!roles.sits_out(1));
        assert!(roles.has_helpers(2) && !roles.is_helper(2));
        assert!(roles.is_helper(5) && roles.has_helpers(5));
        assert!(roles.sits_out(6) && roles.sits_out(7) && roles.sits_out(9));
        assert!(HelperRoles::of([(2, 0)]).hosts.is_empty(), "a pane whose helpers have gone takes part again");
    }

    #[test]
    fn helpers_account_changes_are_drained_in_order_and_bounded() {
        let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = std::sync::Arc::clone(&woken);
        on_account_changed(std::sync::Arc::new(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        for sid in 0..(CHANGES_CAP as u64 + 3) {
            account_changed(AccountChanged { sid, agent: "claude", from: 1, to: 2, at: 0 });
        }
        let got = drain_account_changes();
        assert_eq!(got.len(), CHANGES_CAP);
        assert_eq!(got[0].sid, 3, "the oldest went first");
        assert!(drain_account_changes().is_empty());
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), CHANGES_CAP + 3);
    }
}
