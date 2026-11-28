//! The windows Claude claims have open.  A claim's CLI pane is a shadow
//! pane under one of them, and the account rules (switching, rebalance,
//! leave-early, idle reclaim) leave every such pane alone: moving or
//! restarting it would cut the claim off mid-command, or claim on
//! another account.

use std::sync::Mutex;

/// One claim per account at a time; far more than a machine has.
const MAX: usize = 64;

static WINDOWS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The window panes of the claims running now.
pub(crate) fn windows() -> Vec<u64> {
    WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// A claim opened window `sid`: from now on its panes sit out.  False
/// when the table is full, and the claim must not go on.
pub(super) fn mark(sid: u64) -> bool {
    let mut w = WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    if w.contains(&sid) {
        return true;
    }
    if w.len() >= MAX {
        return false;
    }
    w.push(sid);
    true
}

/// The claim that opened window `sid` closed it.
pub(super) fn unmark(sid: u64) {
    WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).retain(|w| *w != sid);
}
