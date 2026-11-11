//! The sending side, shared by the `marspot-peer` command line and its
//! MCP server: who is live, who a name means, and putting a message
//! (and optionally an idle subscription) into a peer's inbox.
//!
//! The sender is always a session marspot keeps a record for: a Codex
//! session found by its thread id, or a Gemini or Kimi session found as an
//! ancestor of the sending process.  Without such a record there is no
//! reply address and nothing is sent: an anonymous message could not
//! be answered and could not say who it is from.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::envelope::{self, Header, Mode};
use crate::frame::{self, Priority};
use crate::panes::{self, Pane};
use crate::registry::{self, Agent, Entry};
use crate::resolve::Target;

/// Longest message body, in characters; the receiver's own cap.
pub const MAX_BODY: usize = 1_000_000;

/// Every live session in every registry directory, once each.
pub fn live_entries() -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for dir in registry::default_dirs() {
        for e in registry::list_live(&dir) {
            if !out.iter().any(|o| o.pid == e.pid) {
                out.push(e);
            }
        }
    }
    out
}

/// The record marspot keeps for the Codex session with this thread id.
pub fn me_by_thread<'a>(entries: &'a [Entry], thread: &str) -> Result<&'a Entry, String> {
    match entries.iter().find(|e| e.ours && e.session_id.as_deref() == Some(thread)) {
        Some(e) if e.socket.is_some() => Ok(e),
        _ => Err(format!(
            "this Codex session ({thread}) has no inbox: marspot has not bound it to a pane, so it cannot send or be replied to"
        )),
    }
}

/// The record marspot keeps for the Gemini or Kimi session `pid` runs
/// under.
///
/// Neither puts a session id in a tool call, but each starts its MCP
/// servers itself, so the session is the nearest ancestor that such a
/// record names.  Codex records do not count: a Codex call says which
/// thread it is, and one Codex process can hold several.
pub fn me_by_ancestor(entries: &[Entry], pid: i32, parent_of: impl Fn(i32) -> Option<i32>) -> Option<&Entry> {
    let gemini = |e: &&Entry| e.ours && matches!(e.agent, Some(Agent::Gemini | Agent::Kimi)) && e.socket.is_some();
    let found = crate::proc::nearest_ancestor(pid, parent_of, |p| entries.iter().filter(gemini).any(|e| e.agent_pid == Some(p)))?;
    entries.iter().filter(gemini).find(|e| e.agent_pid == Some(found))
}

/// Who a recipient string reached.
pub struct Recipient<'a> {
    pub socket: String,
    pub entry: Option<&'a Entry>,
    /// Set when a full name found a pane that is called something else
    /// now.
    pub note: Option<String>,
}

/// The inbox `to` means: a `uds:` address, a marspot pane in any of the
/// forms `resolve` takes, or the own name of a session that is in no
/// pane.
pub fn resolve<'a>(entries: &'a [Entry], panes: &[Pane], to: &str) -> Result<Recipient<'a>, String> {
    if let Some(path) = to.strip_prefix("uds:") {
        return Ok(Recipient { socket: path.to_string(), entry: entries.iter().find(|e| e.socket.as_deref() == Some(path)), note: None });
    }
    let targets: Vec<Target> = panes.iter().map(Pane::target).collect();
    let miss = match crate::resolve::resolve(to, &targets) {
        Ok(hit) => {
            let p = &panes[hit.index];
            return Ok(Recipient { socket: p.socket.clone(), entry: entries.iter().find(|e| e.pid == p.pid), note: hit.note });
        }
        Err(miss) => miss,
    };
    if !miss.unknown {
        return Err(miss.text);
    }
    let outside: Vec<&Entry> = entries.iter().filter(|e| e.name.as_deref() == Some(to) && panes::of_pid(panes, e.pid).is_none()).collect();
    match outside.as_slice() {
        [e] => match &e.socket {
            Some(s) => Ok(Recipient { socket: s.clone(), entry: Some(e), note: None }),
            None => Err(format!("{to} has no inbox socket")),
        },
        [] => Err(miss.text),
        _ => Err(format!(
            "{} live sessions are named {to:?}; use one of their addresses instead: {}",
            outside.len(),
            outside.iter().filter_map(|e| e.socket.as_deref()).map(|s| format!("uds:{s}")).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub struct Sent {
    pub msg_id: String,
    /// The recipient's name, or the address when it has none.
    pub to: String,
    /// `Some(true)` when an idle subscription was placed, `Some(false)`
    /// when one was asked for and the recipient does not support it.
    pub subscribed: Option<bool>,
    /// What the sender should know about the name it used.
    pub note: Option<String>,
}

/// Send `body` from `me` to `to`.  With `notify_idle`, also ask the
/// recipient for one notice when it next goes idle.  An empty body with
/// `notify_idle` is a subscription alone.
pub fn send(me: &Entry, entries: &[Entry], panes: &[Pane], to: &str, body: &str, priority: Priority, notify_idle: bool) -> Result<Sent, String> {
    if body.trim().is_empty() && !notify_idle {
        return Err("empty message".into());
    }
    if body.chars().count() > MAX_BODY {
        return Err(format!("message longer than {MAX_BODY} characters"));
    }
    let my_socket = me.socket.clone().unwrap_or_default();
    let Recipient { socket: target, entry, note } = resolve(entries, panes, to)?;
    if target == my_socket {
        return Err("that address is this session's own inbox".into());
    }
    let name = entry.map(|e| display_name(e, panes)).unwrap_or_else(|| to.to_string());
    let from = format!("uds:{my_socket}");
    // a record without a class is reported as prompting: claiming
    // bypass for a session we cannot vouch for would get it delivered
    // where it should be held
    let mode = me.class.unwrap_or(Mode::Prompting);
    let msg_id = crate::rand::uuid_v4();
    if !body.trim().is_empty() {
        let header = Header { from: from.clone(), from_name: Some(display_name(me, panes)), from_mode: Some(mode) };
        let content = envelope::encode(&header, body);
        write_line(me.relay.as_deref(), &target, &name, &frame::user_line(&content, &from, priority, &msg_id))?;
    }
    let subscribed = if !notify_idle {
        None
    } else if entry.is_some_and(|e| e.supports("notify_idle")) {
        let id = crate::rand::uuid_v4();
        write_line(me.relay.as_deref(), &target, &name, &frame::notify_when_idle_line(&from, mode.as_str(), &id))?;
        Some(true)
    } else {
        Some(false)
    };
    Ok(Sent { msg_id, to: name, subscribed, note })
}

/// Through the sender's relay when it has one, so the peer sees the
/// connection come from the process its registry record names.
fn write_line(relay: Option<&str>, target: &str, name: &str, line: &str) -> Result<(), String> {
    if let Some(r) = relay {
        return crate::relay::send_via(r, target, line).map_err(|e| format!("cannot reach {name}: {e}"));
    }
    let mut s = UnixStream::connect(target).map_err(|e| format!("cannot reach {name}: {e}"))?;
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    s.write_all(line.as_bytes()).map_err(|e| format!("cannot write to {name}: {e}"))
}

/// What a session is called here: its pane's name when it is in a
/// marspot pane, its own name otherwise.
pub fn display_name(e: &Entry, panes: &[Pane]) -> String {
    panes::of_pid(panes, e.pid)
        .map(|p| p.name.clone())
        .or_else(|| e.name.clone())
        .unwrap_or_else(|| "?".into())
}

/// The session list: one row per peer with the name to send to, the
/// agent, and whether it is busy.  A pane shown under its plain name
/// has its full name at the end of the row.
pub fn listing(entries: &[Entry], panes: &[Pane], me: Option<&Entry>, now_ms: i64) -> String {
    let mut out = String::new();
    if let Some(m) = me {
        out.push_str(&format!(
            "This session is {} — the name other sessions use to message it (it is not listed below; a message to it would be a message to yourself).\n\n",
            display_name(m, panes)
        ));
    }
    let peers: Vec<&Entry> = entries.iter().filter(|e| me.is_none_or(|m| m.pid != e.pid)).collect();
    let names: Vec<String> = peers.iter().map(|e| display_name(e, panes)).collect();
    out.push_str(&format!("Peer sessions ({}):\n", peers.len()));
    for (e, name) in peers.iter().zip(&names) {
        let pane = panes::of_pid(panes, e.pid);
        let kind = pane.and_then(|p| p.agent.as_deref()).unwrap_or(e.kind());
        let own = e.name.as_deref().filter(|n| n != name).map(|n| format!("  ·  session {n}")).unwrap_or_default();
        let shared = names.iter().filter(|n| *n == name).count() > 1;
        let addr = match (&e.socket, shared) {
            (Some(s), true) => format!("  ·  uds:{s}"),
            _ => String::new(),
        };
        let started = e.started_at_ms.map(|t| format!("  ·  started {}", ago(now_ms - t))).unwrap_or_default();
        let full = pane.and_then(Pane::full_name_if_not_shown).map(|f| format!("  ({f})")).unwrap_or_default();
        out.push_str(&format!("  {name}  ·  {kind}  ·  {}{started}{own}{addr}{full}\n", e.status.as_deref().unwrap_or("?")));
    }
    out
}

fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 3600 {
        format!("{}m ago", (s / 60).max(1))
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: i32, name: &str) -> Entry {
        Entry {
            pid,
            session_id: Some(format!("s{pid}")),
            name: Some(name.into()),
            cwd: None,
            socket: Some(format!("/tmp/cc-socks/{pid}.sock")),
            status: Some("idle".into()),
            proc_start: None,
            started_at_ms: Some(0),
            features: Vec::new(),
            ours: false,
            class: None,
            relay: None,
            owner: None,
            agent: None,
            agent_pid: None,
            spool: None,
        }
    }

    /// The MCP server gemini started is told which session it sends
    /// for by where it sits in the process tree.
    #[test]
    fn a_gemini_sender_is_the_ancestor_with_a_gemini_record() {
        let ours = |pid: i32, agent: Agent, agent_pid: i32| Entry {
            ours: true,
            agent: Some(agent),
            agent_pid: Some(agent_pid),
            ..entry(pid, "x")
        };
        // records are keyed by relay pid; the agent's pid is beside it
        let es = vec![ours(900, Agent::Gemini, 30), ours(901, Agent::Codex, 20), entry(10, "claude")];
        // 50 (this server) → 40 (gemini worker) → 30 (gemini) → 20 (codex) → 10 (claude)
        let table = |pid: i32| [(50, 40), (40, 30), (30, 20), (20, 10)].iter().find(|(p, _)| *p == pid).map(|(_, pp)| *pp);
        assert_eq!(me_by_ancestor(&es, 50, table).map(|e| e.pid), Some(900));
        assert_eq!(me_by_ancestor(&es, 30, table), None, "under codex and claude only: neither is found this way");
        let no_inbox = vec![Entry { socket: None, ..ours(900, Agent::Gemini, 30) }];
        assert_eq!(me_by_ancestor(&no_inbox, 50, table), None);
        // kimi starts its servers as direct children
        let kimi = vec![ours(902, Agent::Kimi, 40)];
        assert_eq!(me_by_ancestor(&kimi, 50, table).map(|e| e.pid), Some(902));
    }

    fn pane(base: &str, code: u16, agent: &str, shown: &str, pid: i32) -> Pane {
        Pane { name: shown.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock"), code: Some(code), base: Some(base.into()), agent: Some(agent.into()) }
    }

    #[test]
    fn addresses_and_sessions_outside_any_pane_resolve() {
        let es = vec![entry(1, "gds"), entry(2, "luna"), entry(3, "luna")];
        assert_eq!(resolve(&es, &[], "gds").ok().unwrap().socket, "/tmp/cc-socks/1.sock");
        assert_eq!(resolve(&es, &[], "uds:/tmp/cc-socks/2.sock").ok().unwrap().entry.map(|e| e.pid), Some(2));
        assert_eq!(resolve(&es, &[], "nobody").err().unwrap(), "no pane is named \"nobody\"");
        let err = resolve(&es, &[], "luna").err().unwrap();
        assert!(err.contains("uds:/tmp/cc-socks/2.sock") && err.contains("uds:/tmp/cc-socks/3.sock"));
    }

    /// A session in a pane is reached by the pane's forms and not by
    /// the name it gives itself, which another pane may carry.
    #[test]
    fn a_pane_is_reached_by_its_name_code_and_agent() {
        let es = vec![entry(1, "inputx-6a"), Entry { ours: true, agent: Some(Agent::Codex), ..entry(2, "inputx#9906") }];
        let panes = vec![pane("inputx", 1024, "claude", "inputx#1024", 1), pane("inputx", 9906, "codex", "inputx#9906", 2)];
        let to = |to: &str| resolve(&es, &panes, to).map(|r| r.entry.map(|e| e.pid));
        assert_eq!(to("claude:inputx"), Ok(Some(1)));
        assert_eq!(to("codex:inputx"), Ok(Some(2)));
        assert_eq!(to("inputx#1024"), Ok(Some(1)));
        assert_eq!(to("inputx#9906"), Ok(Some(2)));
        assert!(to("inputx").unwrap_err().contains("inputx#1024 (claude), inputx#9906 (codex)"));
        assert!(to("inputx-6a").unwrap_err().starts_with("no pane is named"));
        let moved = vec![pane("alt", 1024, "claude", "alt", 1)];
        let r = resolve(&es, &moved, "inputx#1024").ok().unwrap();
        assert_eq!((r.socket.as_str(), r.note.as_deref()), ("/tmp/cc-socks/1.sock", Some("inputx#1024 is now called alt#1024")));
    }

    #[test]
    fn the_listing_names_agent_and_full_name() {
        let es = vec![
            entry(1, "gds"),
            entry(2, "marspot-03"),
            Entry { ours: true, agent: Some(Agent::Codex), status: Some("busy".into()), ..entry(3, "marspot#9906") },
            Entry { ours: true, agent: Some(Agent::Kimi), ..entry(4, "alt") },
            entry(5, "luna"),
            entry(6, "luna"),
        ];
        let panes = vec![pane("marspot", 1024, "claude", "marspot#1024", 2), pane("marspot", 9906, "codex", "marspot#9906", 3), pane("alt", 3381, "kimi", "alt", 4)];
        let text = listing(&es, &panes, Some(&es[0]), 26 * 60 * 1000);
        assert!(text.starts_with("This session is gds — "));
        assert!(text.contains("Peer sessions (5):\n"));
        assert!(text.contains("  marspot#1024  ·  claude  ·  idle  ·  started 26m ago  ·  session marspot-03\n"), "{text}");
        assert!(text.contains("  marspot#9906  ·  codex  ·  busy  ·  started 26m ago\n"), "{text}");
        assert!(text.contains("  alt  ·  kimi  ·  idle  ·  started 26m ago  (alt#3381)\n"), "{text}");
        assert!(text.contains("  luna  ·  claude  ·  idle  ·  started 26m ago  ·  uds:/tmp/cc-socks/5.sock\n"), "{text}");
        assert!(!text.contains("  gds  ·"), "this session is not among its peers");
        // the name a message carries as its sender is the one drawn
        assert_eq!(display_name(&es[1], &panes), "marspot#1024");
        assert_eq!(display_name(&es[3], &panes), "alt");
    }
}
