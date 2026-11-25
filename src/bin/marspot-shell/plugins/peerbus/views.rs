//! What other plugins may read of the worker's bindings.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use marspot_peer::registry::Status;

use super::refusal::TurnEnds;
use super::worker::Worker;

/// A pane's codex thread as the worker knows it, for other plugins:
/// the codex plugin restarts a pane's TUI onto the same thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadView {
    pub codex: marspot::pidtree::ProcId,
    pub home: PathBuf,
    pub thread: String,
    /// The daemon says the thread is idle, with no turn running and no
    /// prompt waiting.  Only known for a thread this worker subscribed
    /// to; an unsubscribed one hears no status and reads as not idle.
    pub idle: bool,
    /// How its last turns ended, as the daemon said.
    pub ends: TurnEnds,
}

static THREADS: Mutex<Option<HashMap<u64, ThreadView>>> = Mutex::new(None);

/// Every bound codex pane's thread, by session id.
pub(crate) fn threads() -> HashMap<u64, ThreadView> {
    THREADS.lock().unwrap().clone().unwrap_or_default()
}

pub(crate) fn publish_threads(views: HashMap<u64, ThreadView>) {
    *THREADS.lock().unwrap() = Some(views);
}

impl Worker {
    /// What other plugins may read of the bindings: see `ThreadView`.
    pub(super) fn publish(&self) {
        let views = self
            .bindings
            .iter()
            .map(|(sid, b)| {
                let idle = b.subscribed && b.record.status == Status::Idle && b.active_turn.is_none();
                (*sid, ThreadView { codex: b.codex, home: b.home.clone(), thread: b.thread.clone(), idle, ends: b.replies.ends.clone() })
            })
            .collect();
        publish_threads(views);
    }
}
