//! A helper and the session that started it, which message each other
//! whatever their permission classes: the one asked for the other, and
//! a message says itself that it is not the user's word.  Only that
//! pair: two helpers of one session, or a helper's helper and that
//! session, are like any other two.
//!
//! The sender is the pane whose inbox the message gives as its reply
//! address, the same word its class is taken from.

use marspot_peer::frame::UserFrame;
use marspot_peer::peers;

use super::deliver;
use super::worker::Worker;

/// The pane whose inbox is `socket`: one marspot serves (`ours`), else
/// that of the claude in front of a pane (`claude`).
fn pane_of_inbox<'a>(socket: &str, mut ours: impl Iterator<Item = (u64, &'a str)>, claude: impl FnOnce(&str) -> Option<u64>) -> Option<u64> {
    ours.find(|(_, s)| *s == socket).map(|(sid, _)| sid).or_else(|| claude(socket))
}

fn claude_pane(socket: &str) -> Option<u64> {
    let e = peers::live_entries().into_iter().find(|e| !e.ours && e.socket.as_deref() == Some(socket))?;
    super::names::Panes::look().pane_of(e.pid)
}

impl Worker {
    /// Whether `frame`, at inbox `key`, declares another class than the
    /// session there and comes from its helper or from its parent.
    pub(super) fn is_from_pair(&self, key: u64, frame: &UserFrame) -> bool {
        let codex = self.bindings.iter().map(|(sid, b)| (*sid, b.key, b.class));
        let spooled = self.gemini.iter().map(|(sid, g)| (*sid, g.key, g.class));
        let Some((sid, _, class)) = codex.chain(spooled).find(|(_, k, _)| *k == key) else { return false };
        // most messages are settled by their class alone
        if !deliver::other_class(frame, class) {
            return false;
        }
        let Some(from) = deliver::reply_address(frame) else { return false };
        let codex = self.bindings.iter().map(|(sid, b)| (*sid, b.record.socket.as_str()));
        let spooled = self.gemini.iter().map(|(sid, g)| (*sid, g.record.socket.as_str()));
        let sender = pane_of_inbox(from.trim_start_matches("uds:"), codex.chain(spooled), claude_pane);
        sender.is_some_and(|s| crate::plugins::helpers::is_pair(s, sid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_address_names_the_pane_of_its_inbox() {
        let ours = || [(1, "/s/1.sock"), (2, "/s/2.sock")].into_iter();
        let claude = |s: &str| (s == "/s/9.sock").then_some(9);
        assert_eq!(pane_of_inbox("/s/2.sock", ours(), claude), Some(2));
        assert_eq!(pane_of_inbox("/s/9.sock", ours(), claude), Some(9));
        assert_eq!(pane_of_inbox("/s/22.sock", ours(), claude), None);
        assert_eq!(claude_pane("/nowhere/no.sock"), None);
    }
}
