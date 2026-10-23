//! The sending side, shared by the `marspot-peer` command line and its
//! MCP server: who is live, who a name means, and putting a message
//! (and optionally an idle subscription) into a peer's inbox.
//!
//! The sender is always a Codex session marspot keeps a record for,
//! found by its thread id.  Without such a record there is no reply
//! address and nothing is sent: an anonymous message could not be
//! answered and could not say who it is from.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::envelope::{self, Header, Mode};
use crate::frame::{self, Priority};
use crate::panes::{self, Pane};
use crate::registry::{self, Entry};

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

/// The inbox `to` means: a `uds:` address, the name of a marspot pane,
/// a session's own name, or a name it had.  A pane name wins: it is the
/// name the user sees.  A current name wins over a former one, so a
/// name that has moved to another pane reaches that pane.
pub fn resolve<'a>(entries: &'a [Entry], panes: &[Pane], to: &str) -> Result<(String, Option<&'a Entry>), String> {
    if let Some(path) = to.strip_prefix("uds:") {
        return Ok((path.to_string(), entries.iter().find(|e| e.socket.as_deref() == Some(path))));
    }
    if let Some(p) = panes::find(panes, to)? {
        return Ok((p.socket.clone(), entries.iter().find(|e| e.pid == p.pid)));
    }
    let current: Vec<&Entry> = entries.iter().filter(|e| e.name.as_deref() == Some(to)).collect();
    let hits = if current.is_empty() { entries.iter().filter(|e| e.answers_to(to)).collect() } else { current };
    match hits.as_slice() {
        [e] => match &e.socket {
            Some(s) => Ok((s.clone(), Some(e))),
            None => Err(format!("{to} has no inbox socket")),
        },
        [] => Err(format!("no live session named {to:?}; list the sessions to see who is there")),
        _ => Err(format!(
            "{} live sessions are named {to:?}; use one of their addresses instead: {}",
            hits.len(),
            hits.iter().filter_map(|e| e.socket.as_deref()).map(|s| format!("uds:{s}")).collect::<Vec<_>>().join(", ")
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
    let (target, entry) = resolve(entries, panes, to)?;
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
    Ok(Sent { msg_id, to: name, subscribed })
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

/// The session list in the shape Claude Code's `ListAgents` prints, so
/// a model reads the same thing in either tool.  A session in a pane is
/// listed by the pane's name, with its own name after it when the two
/// differ.
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
        let own = e.name.as_deref().filter(|n| n != name).map(|n| format!("  ·  session {n}")).unwrap_or_default();
        let shared = names.iter().filter(|n| *n == name).count() > 1;
        let addr = match (&e.socket, shared) {
            (Some(s), true) => format!("  ·  uds:{s}"),
            _ => String::new(),
        };
        let started = e.started_at_ms.map(|t| format!("  ·  started {}", ago(now_ms - t))).unwrap_or_default();
        out.push_str(&format!("  {name}  ·  interactive  ·  {}{started}{own}{addr}\n", e.status.as_deref().unwrap_or("?")));
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

    fn entry(pid: i32, name: &str, former: &[&str]) -> Entry {
        Entry {
            pid,
            session_id: Some(format!("s{pid}")),
            name: Some(name.into()),
            former_names: former.iter().map(|s| s.to_string()).collect(),
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
        }
    }

    #[test]
    fn names_former_names_and_addresses_resolve() {
        let es = vec![entry(1, "gds", &["codex-gds"]), entry(2, "spg#1", &["spg"]), entry(3, "spg", &[])];
        assert_eq!(resolve(&es, &[], "gds").unwrap().0, "/tmp/cc-socks/1.sock");
        assert_eq!(resolve(&es, &[], "codex-gds").unwrap().0, "/tmp/cc-socks/1.sock");
        // `spg` is a current name of 3 and a former one of 2: the current wins
        assert_eq!(resolve(&es, &[], "spg").unwrap().0, "/tmp/cc-socks/3.sock");
        assert_eq!(resolve(&es, &[], "uds:/tmp/cc-socks/2.sock").unwrap().1.map(|e| e.pid), Some(2));
        assert!(resolve(&es, &[], "nobody").is_err());
        let twins = vec![entry(1, "x", &[]), entry(2, "x", &[])];
        let err = resolve(&twins, &[], "x").unwrap_err();
        assert!(err.contains("uds:/tmp/cc-socks/1.sock") && err.contains("uds:/tmp/cc-socks/2.sock"));
    }

    /// A pane's name reaches the session in it, and wins over a session
    /// that happens to call itself the same.
    #[test]
    fn a_pane_name_wins() {
        let es = vec![entry(1, "inputx-6a", &[]), entry(2, "inputx", &[])];
        let panes = vec![Pane { name: "inputx".into(), pid: 1, socket: "/tmp/cc-socks/1.sock".into() }];
        let (sock, e) = resolve(&es, &panes, "inputx").unwrap();
        assert_eq!((sock.as_str(), e.map(|e| e.pid)), ("/tmp/cc-socks/1.sock", Some(1)));
        assert_eq!(resolve(&es, &panes, "inputx-6a").unwrap().0, "/tmp/cc-socks/1.sock", "its own name still works");
        let text = listing(&es, &panes, None, 0);
        assert!(text.contains("  inputx  ·  interactive  ·  idle  ·  started 1m ago  ·  session inputx-6a"), "{text}");
    }

    #[test]
    fn the_listing_reads_like_claude_codes() {
        let es = vec![entry(1, "gds", &[]), entry(2, "luna", &[]), entry(3, "luna", &[])];
        let text = listing(&es, &[], Some(&es[0]), 26 * 60 * 1000);
        assert!(text.starts_with("This session is gds — "));
        assert!(text.contains("Peer sessions (2):\n"));
        assert!(text.contains("  luna  ·  interactive  ·  idle  ·  started 26m ago  ·  uds:/tmp/cc-socks/2.sock\n"));
        assert!(!text.contains("  gds  ·"), "this session is not among its peers");
    }
}
