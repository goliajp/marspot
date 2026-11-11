//! Which panes run a kimi whose session is known, and what each
//! session is doing, for the peerbus plugin: it gives each of them a
//! name other agent sessions can message, and tells the ones that asked
//! when a session goes idle.  Published from this plugin's tick, which
//! already works both out.

use std::sync::Mutex;

use marspot_peer::envelope::Mode;
use marspot_peer::registry::Status;

use crate::plugins::pidtree;

/// One pane's kimi and the session it has open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Peer {
    pub sid: u64,
    pub kimi: pidtree::ProcId,
    pub session: String,
    pub cwd: String,
    pub status: Status,
    pub class: Mode,
    /// The number of the session's last ended turn.
    pub ended: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Peers {
    pub bound: Vec<Peer>,
    /// Every kimi pane is in `bound`.  False while some kimi's session
    /// cannot be told, so nobody takes a missing session for an ended
    /// one.  A kimi that has not been given its first prompt has no
    /// session and does not count against this.
    pub all_known: bool,
}

/// What is typed into an idle kimi pane that has mail waiting.  It
/// shows in the conversation as the user's own line, so it says what
/// is happening; the messages themselves are put in front of it by the
/// hook the line sets off.
pub(super) const WAKE_LINE: &str = "Read the messages that just arrived from other agent sessions.";

static PEERS: Mutex<Option<Peers>> = Mutex::new(None);

/// The kimi panes as of the last tick; None before the first.
pub(crate) fn peers() -> Option<Peers> {
    PEERS.lock().unwrap().clone()
}

pub(super) fn publish(peers: Peers) {
    *PEERS.lock().unwrap() = Some(peers);
}
