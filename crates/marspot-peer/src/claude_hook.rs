//! `marspot-peer claude-hook`: Claude Code hooks that let a Claude
//! session address peers by marspot pane name, the way a Codex session
//! does with the MCP tools.
//!
//! Claude Code resolves `SendMessage`'s `to` against the names sessions
//! give themselves.  Before the call, a pane name is turned into the
//! `uds:` address of the session in that pane, which Claude Code takes
//! as it takes a reply address.  After `ListAgents`, the pane names are
//! attached to the listing as context.  Anything that is not a pane
//! name is left exactly as it was, so outside marspot, and for every
//! other name, the hook does nothing.

use crate::json::{self, Value, obj};
use crate::panes::{self, Pane};
use crate::peers;
use crate::registry::Entry;

/// The hook's answer to one event, as the JSON Claude Code reads on
/// stdout; None to leave the call alone.
pub fn answer(input: &str) -> Option<String> {
    let v = json::parse(input).ok()?;
    let event = v.str_at("hook_event_name")?;
    let tool = v.str_at("tool_name")?;
    let entries = peers::live_entries();
    let panes = panes::live(&entries);
    if panes.is_empty() {
        return None;
    }
    let out = match (event, tool) {
        ("PreToolUse", "SendMessage") => before_send(v.get("tool_input")?, &panes)?,
        ("PostToolUse", "ListAgents") => {
            let me = v.str_at("session_id").and_then(|s| entries.iter().find(|e| !e.ours && e.session_id.as_deref() == Some(s)));
            obj([("hookEventName", Value::from("PostToolUse")), ("additionalContext", Value::from(pane_context(&entries, &panes, me)))])
        }
        _ => return None,
    };
    Some(obj([("hookSpecificOutput", out)]).to_json())
}

fn before_send(input: &Value, panes: &[Pane]) -> Option<Value> {
    let to = input.str_at("to")?;
    if to.starts_with("uds:") {
        return None;
    }
    match panes::find(panes, to) {
        Ok(None) => None,
        Ok(Some(p)) => {
            let mut rewritten = input.clone();
            rewritten.set("to", Value::from(format!("uds:{}", p.socket)));
            Some(obj([("hookEventName", Value::from("PreToolUse")), ("updatedInput", rewritten)]))
        }
        Err(why) => Some(obj([
            ("hookEventName", Value::from("PreToolUse")),
            ("permissionDecision", Value::from("deny")),
            ("permissionDecisionReason", Value::from(why)),
        ])),
    }
}

/// The pane names, and what each is called in the listing above.
pub fn pane_context(entries: &[Entry], panes: &[Pane], me: Option<&Entry>) -> String {
    let mut out = String::from(
        "marspot pane names. Each is also an address: pass it as SendMessage `to` and it reaches the session in that pane.\n",
    );
    let mut rows: Vec<&Pane> = panes.iter().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    for p in rows {
        let Some(e) = entries.iter().find(|e| e.pid == p.pid) else { continue };
        let kind = if e.ours { "Codex" } else { "Claude Code" };
        let what = if me.is_some_and(|m| m.pid == e.pid) {
            "this session".to_string()
        } else {
            match e.name.as_deref() {
                Some(n) if n != p.name => format!("{kind} session, listed above as \"{n}\""),
                _ => format!("{kind} session"),
            }
        };
        out.push_str(&format!("  {} → {what}\n", p.name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(name: &str, pid: i32) -> Pane {
        Pane { name: name.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock") }
    }

    #[test]
    fn a_pane_name_becomes_its_sessions_address() {
        let panes = vec![pane("gds", 1), pane("spg", 2), pane("spg", 3)];
        let input = json::parse(r#"{"to":"gds","message":"hi","summary":"s"}"#).unwrap();
        let out = before_send(&input, &panes).unwrap();
        let updated = out.get("updatedInput").unwrap();
        assert_eq!(updated.str_at("to"), Some("uds:/tmp/cc-socks/1.sock"));
        assert_eq!(updated.str_at("message"), Some("hi"), "the rest of the call is kept");
        // not a pane name, or already an address: left alone
        assert!(before_send(&json::parse(r#"{"to":"luna-7e","message":"x"}"#).unwrap(), &panes).is_none());
        assert!(before_send(&json::parse(r#"{"to":"uds:/a.sock","message":"x"}"#).unwrap(), &panes).is_none());
        // two panes of one name: refused with the addresses to pick from
        let out = before_send(&json::parse(r#"{"to":"spg","message":"x"}"#).unwrap(), &panes).unwrap();
        assert_eq!(out.str_at("permissionDecision"), Some("deny"));
        assert!(out.str_at("permissionDecisionReason").unwrap().contains("uds:/tmp/cc-socks/3.sock"));
    }

    #[test]
    fn the_context_maps_pane_names_to_listed_names() {
        let e = |pid: i32, name: &str, ours: bool| Entry {
            pid,
            session_id: None,
            name: Some(name.into()),
            former_names: Vec::new(),
            cwd: None,
            socket: Some(format!("/tmp/cc-socks/{pid}.sock")),
            status: None,
            proc_start: None,
            started_at_ms: None,
            features: Vec::new(),
            ours,
            class: None,
            relay: None,
            owner: None,
        };
        let entries = vec![e(1, "gds", true), e(2, "inputx-6a", false), e(3, "marspot-03", false)];
        let panes = vec![pane("gds", 1), pane("inputx", 2), pane("marspot", 3)];
        let text = pane_context(&entries, &panes, Some(&entries[2]));
        assert!(text.contains("  gds → Codex session\n"), "{text}");
        assert!(text.contains("  inputx → Claude Code session, listed above as \"inputx-6a\"\n"), "{text}");
        assert!(text.contains("  marspot → this session\n"), "{text}");
    }
}
