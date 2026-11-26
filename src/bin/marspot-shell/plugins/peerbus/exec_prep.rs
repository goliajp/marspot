//! Closing the inboxes before the shell replaces its own image.
//!
//! Each inbox's relay reads its stdin until EOF.  `exec` closes the
//! write end, so a relay left open ends right after it, as a child the
//! new image knows nothing of.  Closed here instead, each relay is
//! waited for; the new image's worker opens the inboxes and writes the
//! records again.  Nothing else of the worker stops: the exec ends it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use marspot_term::{lx_info, lx_warn};

use super::worker::{Event, Worker};

/// How long the main thread waits for the worker; one relay closes in
/// a few milliseconds.
const WAIT: Duration = Duration::from_secs(5);
/// Between offers to a full channel, on this one path before an exec.
const SEND_RETRY: Duration = Duration::from_millis(10);

static WORKER: Mutex<Option<SyncSender<Event>>> = Mutex::new(None);
/// The inboxes were closed and the exec did not happen: the plugin
/// hands the worker its panes again.
static REOPEN: AtomicBool = AtomicBool::new(false);

pub(super) fn connect(tx: Option<SyncSender<Event>>) {
    *WORKER.lock().unwrap() = tx;
}

/// Have the shell's exec close the inboxes first, and hand the panes
/// back when it failed.  At start-up.
pub(super) fn register() {
    crate::platform::exec_hooks::register(crate::platform::exec_hooks::ExecHook {
        name: "peerbus",
        before: close_inboxes,
        failed: Some(exec_failed),
    });
}

/// Close every inbox and wait for its relay.  Returns once the worker
/// has, or after [`WAIT`]; at once with no worker.  The exec goes ahead
/// either way: the new image waits for what is left (`inherited`).
fn close_inboxes() {
    close_inboxes_within(WAIT)
}

fn close_inboxes_within(wait: Duration) {
    let Some(tx) = WORKER.lock().unwrap().clone() else { return };
    let deadline = Instant::now() + wait;
    let (done_tx, done_rx) = sync_channel(1);
    let mut ev = Event::CloseInboxes(done_tx);
    // never a blocking send: the channel is bounded, and a worker stuck
    // with it full would hold the exec, with the core already shut down
    loop {
        match tx.try_send(ev) {
            Ok(()) => break,
            Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                ev = back;
                std::thread::sleep(SEND_RETRY);
            }
            Err(TrySendError::Full(_)) => {
                return lx_warn!("peerbus.exec_prep", "the worker's channel stayed full; exec with the inboxes open", worker = "alive");
            }
            Err(TrySendError::Disconnected(_)) => {
                return lx_warn!("peerbus.exec_prep", "the worker is gone; exec with the inboxes open", worker = "gone");
            }
        }
    }
    match done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(n) => lx_info!("peerbus.exec_prep", "inboxes closed before exec", closed = n),
        Err(RecvTimeoutError::Timeout) => {
            lx_warn!("peerbus.exec_prep", "the worker did not close the inboxes in time", worker = "alive")
        }
        Err(RecvTimeoutError::Disconnected) => {
            lx_warn!("peerbus.exec_prep", "the worker ended before it closed the inboxes", worker = "gone")
        }
    }
}

/// The exec that [`close_inboxes`] prepared for failed.
fn exec_failed() {
    REOPEN.store(true, Ordering::Release);
}

/// Taken by the plugin's tick.
pub(super) fn take_reopen() -> bool {
    REOPEN.swap(false, Ordering::AcqRel)
}

impl Worker {
    /// The worker's half of [`close_inboxes`]: the bindings go without
    /// their records or notices, which the next image puts right.
    pub(super) fn close_inboxes(&mut self) -> usize {
        let n = self.bindings.len() + self.gemini.len();
        for (_, b) in self.bindings.drain() {
            b.inbox.close();
        }
        for (_, g) in self.gemini.drain() {
            g.inbox.close();
        }
        // what the plugin sends after a failed exec binds them again
        self.wants.clear();
        n
    }
}

#[cfg(test)]
#[path = "exec_prep_tests.rs"]
mod tests;
