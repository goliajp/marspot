//! Whether something typed is waiting for the loop.
//!
//! The loop parses output for up to a frame's time before it looks at
//! its events again, and a key that arrived just after it started
//! waited that long to reach the pane's program: 4 ms at the median
//! under a flood the parser cannot keep up with.  The control reader
//! raises this as it queues a key, and the parser looks at it every few
//! kilobytes: one load of a shared byte, no system call.

use std::sync::atomic::{AtomicBool, Ordering};

static WAITING: AtomicBool = AtomicBool::new(false);

/// What the parser looks at between two slices of output.
pub(crate) fn waiting() -> &'static AtomicBool {
    &WAITING
}

/// A key, a paste or injected input has just been queued for the loop.
///
/// Raised after the event is queued, not before: a loop that saw it
/// raised and found nothing to take would lower it and parse on.
pub(crate) fn arrived() {
    WAITING.store(true, Ordering::Release);
}

/// The loop is about to take everything queued for it.
pub(crate) fn taken() {
    WAITING.store(false, Ordering::Release);
}
