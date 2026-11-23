//! A helper and the session that started it, which message each other
//! whatever their permission classes: the one asked for the other, and
//! a message says itself that it is not the user's word.  Only that
//! pair: two helpers of one session, or a helper's helper and that
//! session, are like any other two.
//!
//! The sender is the pane whose inbox the message gives as its reply
//! address, the same word its class is taken from.

use marspot::pidtree::ProcId;
use marspot_peer::frame::UserFrame;
use marspot_peer::peers;
use marspot_peer::registry::Entry;

use super::deliver;
use super::worker::Worker;
use crate::observe::{Proc, Session};

/// The pane whose inbox is `socket`: one marspot serves (`ours`), else
/// that of the claude in front of a pane (`claude`).
fn pane_of_inbox<'a>(socket: &str, mut ours: impl Iterator<Item = (u64, &'a str)>, claude: impl FnOnce(&str) -> Option<u64>) -> Option<u64> {
    ours.find(|(_, s)| *s == socket).map(|(sid, _)| sid).or_else(|| claude(socket))
}

fn claude_pane(socket: &str) -> Option<u64> {
    let e = peers::live_entries().into_iter().find(|e| !e.ours && e.socket.as_deref() == Some(socket))?;
    super::names::Panes::look().pane_of(e.pid)
}

/// A conversation, and the agent process in it.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::plugins) struct Conversation {
    pub agent: ProcId,
    pub id: String,
}

/// The conversation pane `sid`'s agent is in, as its record names it.
pub(in crate::plugins) fn conversation_of(sid: u64) -> Option<Conversation> {
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    conversation_in(obs.session(sid)?, &peers::live_entries())
}

/// That of the agent in front of the pane: claude's own record, or the
/// one marspot keeps.
fn conversation_in(s: &Session, entries: &[Entry]) -> Option<Conversation> {
    let record = |p: &Proc| {
        let is_claude = p.name() == "claude";
        entries.iter().find(|e| if e.ours { e.agent_pid == Some(p.row.pid) } else { is_claude && e.pid == p.row.pid })
    };
    let front = s.agent(|p| record(p).is_some())?;
    Some(Conversation { agent: front.row.id(), id: record(front)?.session_id.clone()? })
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
        sender.is_some_and(|s| crate::plugins::helpers::is_pair(s, sid, conversation_of))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_conversation_of_a_pane_is_its_agents() {
        use marspot_peer::registry::Agent;
        let entry = |pid, agent: Option<Agent>, conv: &str| Entry {
            pid,
            session_id: Some(conv.into()),
            name: None,
            cwd: None,
            socket: None,
            status: None,
            proc_start: None,
            started_at_ms: None,
            features: Vec::new(),
            ours: agent.is_some(),
            class: None,
            relay: None,
            owner: None,
            agent,
            agent_pid: agent.map(|_| pid + 100),
            spool: None,
        };
        let all = [entry(5, None, "claude's"), entry(6, Some(Agent::Codex), "codex's"), entry(7, None, "a tool's")];
        let proc = |pid, name: &str| Proc { row: crate::observe::tests::row(pid, 1, name), argv: Some(vec![name.to_string()].into()) };
        let procs = || vec![proc(5, "claude"), proc(7, "vim"), proc(106, "codex")];
        let front = |front, procs| conversation_in(&crate::observe::tests::session_with(procs, front), &all).map(|c| (c.agent.pid, c.id));
        assert_eq!(front(5, procs()), Some((5, "claude's".into())));
        assert_eq!(front(106, procs()), Some((106, "codex's".into())));
        // an agent in the background is not the pane's, whatever is in front
        assert_eq!(front(0, procs()), None, "the shell");
        assert_eq!(front(7, procs()), None, "a program that is no agent");
        assert_eq!(front(0, Vec::new()), None);
    }

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
