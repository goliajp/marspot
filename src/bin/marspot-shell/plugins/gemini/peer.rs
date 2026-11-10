//! Which panes run a gemini whose session is known, for the peerbus
//! plugin: it gives each of them a name other agent sessions can
//! message.  This plugin already works out the session of every gemini
//! pane, so the answer is published from its tick rather than worked
//! out a second time.

use std::sync::Mutex;

use crate::plugins::pidtree;

/// One pane's gemini and the session it is writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    pub sid: u64,
    /// The gemini nearest the shell: every process gemini starts, its
    /// relaunched self included, descends from it.
    pub gemini: pidtree::ProcId,
    pub session: String,
    pub cwd: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Peers {
    pub bound: Vec<Peer>,
    /// Every gemini pane is in `bound`.  False while some gemini's
    /// session cannot be told yet, so nobody takes a missing session
    /// for a finished one.
    pub all_known: bool,
}

static PEERS: Mutex<Option<Peers>> = Mutex::new(None);

/// The gemini panes as of the last tick; None before the first.
pub(crate) fn peers() -> Option<Peers> {
    PEERS.lock().unwrap().clone()
}

pub(super) fn publish(peers: Peers) {
    *PEERS.lock().unwrap() = Some(peers);
}
