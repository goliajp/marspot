//! Which agent accounts have a pane at work, as the agents' scans last
//! saw it, for a claim that waits until its account rests.
//!
//! Each scan replaces its agent's rows; a waiting claim is woken by
//! that, never by a timer of its own.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// One row per agent and account at most: two agents, 256 accounts each.
const MAX_ROWS: usize = 512;

struct Table {
    /// `(agent, account, every pane on it rests)`.
    rows: Vec<(&'static str, u8, bool)>,
}

static TABLE: Mutex<Table> = Mutex::new(Table { rows: Vec::new() });
static CHANGED: Condvar = Condvar::new();

/// What one scan of `agent` saw: each account with a pane on it, and
/// whether every pane there rests.  An account left out has no pane.
pub(crate) fn publish(agent: &'static str, accounts: impl IntoIterator<Item = (u8, bool)>) {
    let mut t = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    t.rows.retain(|(a, _, _)| *a != agent);
    for (profile, rests) in accounts {
        if t.rows.len() >= MAX_ROWS {
            break;
        }
        match t.rows.iter_mut().find(|(a, p, _)| *a == agent && *p == profile) {
            Some(row) => row.2 &= rests,
            None => t.rows.push((agent, profile, rests)),
        }
    }
    drop(t);
    CHANGED.notify_all();
}

fn rests(t: &Table, agent: &str, profile: u8) -> bool {
    !t.rows.iter().any(|(a, p, r)| *a == agent && *p == profile && !r)
}

/// Whether every pane on the account rests, now or within `within`.
pub(crate) fn wait_idle(agent: &str, profile: u8, within: Duration) -> bool {
    let until = Instant::now() + within;
    let mut t = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if rests(&t, agent, profile) {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        t = CHANGED.wait_timeout(t, left).unwrap_or_else(|p| p.into_inner()).0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_waits_for_its_account_to_rest_and_no_longer_than_it_may() {
        // an agent name of its own: other tests publish to the same table
        const AGENT: &str = "test-idle";
        publish(AGENT, [(1, true), (2, false), (2, true)]);
        assert!(wait_idle(AGENT, 1, Duration::ZERO));
        assert!(wait_idle(AGENT, 9, Duration::ZERO), "an account with no pane rests");
        let t = Instant::now();
        assert!(!wait_idle(AGENT, 2, Duration::from_millis(50)), "one pane of two works");
        assert!(t.elapsed() >= Duration::from_millis(50));

        let waiter = std::thread::spawn(|| wait_idle(AGENT, 2, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(30));
        publish(AGENT, [(2, true)]);
        assert!(waiter.join().unwrap(), "woken by the scan that saw it rest");
    }
}
