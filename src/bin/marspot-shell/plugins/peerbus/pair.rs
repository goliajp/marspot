//! A helper and the session that started it, which message each other
//! whatever their permission classes: the one asked for the other, and
//! a message says itself that it is not the user's word.  Only that
//! pair: two helpers of one session, or a helper's helper and that
//! session, are like any other two.
//!
//! The sender is the pane whose inbox the message gives as its reply
//! address, the same word its class is taken from.

use std::path::{Path, PathBuf};

use marspot::pidtree::ProcId;
use marspot_peer::frame::UserFrame;
use marspot_peer::peers;
use marspot_peer::registry::{self, Entry};

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

/// The files agent `pid`'s conversation is named in, in registry
/// directory `dir`: claude's own record, and the one marspot keeps for
/// it once it keeps one.
pub(in crate::plugins) fn record_files(dir: &Path, pid: i32) -> Vec<PathBuf> {
    std::iter::once(pid).chain(super::records::filed_under(pid)).map(|p| registry::record_path(dir, p)).collect()
}

/// The conversation agent `agent` is in, as its own record in `dirs`
/// names it; read from those files alone, whatever is in front of its
/// pane.
pub(in crate::plugins) fn conversation_of_agent(dirs: &[PathBuf], agent: ProcId) -> Option<Conversation> {
    let filed = super::records::filed_under(agent.pid);
    let names = |e: &Entry| e.is_live() && if e.ours { e.agent_pid == Some(agent.pid) } else { e.pid == agent.pid };
    let mut records = dirs.iter().flat_map(|d| std::iter::once(agent.pid).chain(filed).filter_map(|p| registry::read(d, p)));
    let id = records.find(names)?.session_id?;
    Some(Conversation { agent, id })
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

    /// Read from the agent's own records alone: claude's under its pid,
    /// ours under its relay's, once written.
    #[test]
    fn the_conversation_of_an_agent_is_its_own_records() {
        use marspot_peer::envelope::Mode;
        use marspot_peer::registry::{Agent, Record, Status};
        let dir = std::env::temp_dir().join(format!("marspot-agent-records-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let me = marspot::pidtree::proc_id(std::process::id() as i32).unwrap();
        let relay = std::os::unix::process::parent_id() as i32;
        assert_eq!(record_files(&dir, me.pid), vec![registry::record_path(&dir, me.pid)]);
        assert_eq!(conversation_of_agent(std::slice::from_ref(&dir), me), None);
        let r = Record {
            pid: relay,
            agent: Agent::Codex,
            agent_pid: me.pid,
            relay: "/tmp/r.sock".into(),
            session_id: "thread-1".into(),
            cwd: "/tmp".into(),
            started_at_ms: 1,
            proc_start: marspot_peer::proc::lstart(relay).unwrap(),
            version: String::new(),
            name: "x".into(),
            name_since_ms: 1,
            socket: "/tmp/cc-socks/x.sock".into(),
            status: Status::Idle,
            class: Mode::Bypass,
            owner: "/tmp/owner".into(),
            spool: None,
        };
        super::super::records::write_record(std::slice::from_ref(&dir), &r);
        assert_eq!(record_files(&dir, me.pid), vec![registry::record_path(&dir, me.pid), registry::record_path(&dir, relay)]);
        let found = conversation_of_agent(std::slice::from_ref(&dir), me);
        assert_eq!(found, Some(Conversation { agent: me, id: "thread-1".into() }));
        let other = marspot::pidtree::ProcId { pid: relay, start_us: 0 };
        assert_eq!(conversation_of_agent(std::slice::from_ref(&dir), other), None, "a relay is no agent");
        std::fs::remove_dir_all(&dir).unwrap();
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
