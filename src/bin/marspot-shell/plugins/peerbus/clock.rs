//! The worker's clocks: when it next has something owed, and doing
//! what is owed by now.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Instant;

use super::worker::{Event, Worker};

impl Worker {
    /// The next event, or a timeout once the earliest clock is due;
    /// with nothing owed on a clock, waits for the event.
    pub(super) fn next_event(&mut self) -> Result<Event, RecvTimeoutError> {
        let idle = self.idle_checks.iter().map(|(_, t)| *t).min();
        match [self.retry_at, idle, self.moves.due()].into_iter().flatten().min() {
            Some(due) => self.rx.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }

    /// Run every clock that is due.  Called when a wait times out and
    /// after every event, so a stream of events cannot hold them off.
    pub(super) fn timers_due(&mut self) {
        let now = Instant::now();
        if self.retry_at.is_some_and(|t| t <= now) {
            self.retry_at = None;
            self.refresh();
            self.publish();
        }
        if self.idle_checks.iter().any(|(_, t)| *t <= now) {
            self.idle_checks_due(now);
        }
        if self.moves.due().is_some_and(|t| t <= now) {
            self.moves_due(now);
        }
    }
}
