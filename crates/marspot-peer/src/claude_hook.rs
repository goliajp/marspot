//! `marspot-peer claude-hook`: Claude Code hooks that let a Claude
//! session address peers by marspot pane name, the way a Codex session
//! does with the MCP tools.
//!
//! Claude Code resolves `SendMessage`'s `to` against the names sessions
//! give themselves.  Before the call, a pane name is turned into the
//! `uds:` address of the session in that pane, which Claude Code takes
//! as it takes a reply address.  After `ListAgents`, the pane names are
//! attached to the listing as context.  A name no pane has is left
//! exactly as it was, so outside marspot, and for every other name,
//! the hook does nothing; one written with a code or an agent in front
//! can only be a pane's, and is refused when no pane fits.

use crate::json::{self, Value, obj};
use crate::panes::{self, Pane};
use crate::peers;
use crate::registry::Entry;
use crate::resolve::{self, Target};

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
    let targets: Vec<Target> = panes.iter().map(Pane::target).collect();
    match resolve::resolve(to, &targets) {
        Ok(hit) => {
            let mut rewritten = input.clone();
            rewritten.set("to", Value::from(format!("uds:{}", panes[hit.index].socket)));
            let mut out = obj([("hookEventName", Value::from("PreToolUse")), ("updatedInput", rewritten)]);
            if let Some(note) = hit.note {
                out.set("additionalContext", Value::from(format!("{note}.")));
            }
            Some(out)
        }
        Err(miss) if miss.unknown && !resolve::is_pane_form(to) => None,
        Err(miss) => Some(obj([
            ("hookEventName", Value::from("PreToolUse")),
            ("permissionDecision", Value::from("deny")),
            ("permissionDecisionReason", Value::from(miss.text)),
        ])),
    }
}

/// The panes, one per row: the name to send to, the agent, whether it
/// is busy, the full name when the row leads with the plain one, and
/// what the session is called in the listing above.
pub fn pane_context(entries: &[Entry], panes: &[Pane], me: Option<&Entry>) -> String {
    let mut out = format!(
        "marspot pane names. Each is also an address: pass it as SendMessage `to` and it reaches the session in that pane.\n{}\n",
        resolve::FORMS
    );
    let mut rows: Vec<&Pane> = panes.iter().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    for p in rows {
        let Some(e) = entries.iter().find(|e| e.pid == p.pid) else { continue };
        let kind = p.agent.as_deref().unwrap_or(e.kind());
        let full = p.full_name_if_not_shown().map(|f| format!("  ({f})")).unwrap_or_default();
        let what = if me.is_some_and(|m| m.pid == e.pid) {
            "this session".to_string()
        } else {
            match e.name.as_deref() {
                Some(n) if n != p.name => format!("listed above as \"{n}\""),
                _ => "listed above under the same name".to_string(),
            }
        };
        out.push_str(&format!("  {}  {kind}  {}{full} → {what}\n", p.name, e.status.as_deref().unwrap_or("?")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Agent;

    fn pane(base: &str, code: u16, agent: &str, shown: &str, pid: i32) -> Pane {
        Pane { name: shown.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock"), code: Some(code), base: Some(base.into()), agent: Some(agent.into()) }
    }

    fn send(to: &str, panes: &[Pane]) -> Option<Value> {
        before_send(&obj([("to", Value::from(to)), ("message", Value::from("hi")), ("summary", Value::from("s"))]), panes)
    }

    fn address(out: Option<Value>) -> Option<String> {
        out?.get("updatedInput")?.str_at("to").map(str::to_string)
    }

    fn refusal(out: Option<Value>) -> Option<String> {
        let out = out?;
        assert_eq!(out.str_at("permissionDecision"), Some("deny"));
        out.str_at("permissionDecisionReason").map(str::to_string)
    }

    #[test]
    fn a_pane_name_becomes_its_sessions_address() {
        let panes = vec![pane("gds", 2000, "claude", "gds", 1), pane("spg", 1024, "claude", "spg#1024", 2), pane("spg", 9906, "codex", "spg#9906", 3)];
        let out = send("gds", &panes).unwrap();
        let updated = out.get("updatedInput").unwrap();
        assert_eq!(updated.str_at("to"), Some("uds:/tmp/cc-socks/1.sock"));
        assert_eq!(updated.str_at("message"), Some("hi"), "the rest of the call is kept");
        assert_eq!(address(send("gds#2000", &panes)).as_deref(), Some("uds:/tmp/cc-socks/1.sock"));
        assert_eq!(address(send("claude:spg", &panes)).as_deref(), Some("uds:/tmp/cc-socks/2.sock"));
        assert_eq!(address(send("codex:spg", &panes)).as_deref(), Some("uds:/tmp/cc-socks/3.sock"));
        assert_eq!(address(send("spg#9906", &panes)).as_deref(), Some("uds:/tmp/cc-socks/3.sock"));
        // not a pane name, or already an address: left alone
        assert!(send("luna-7e", &panes).is_none());
        assert!(send("uds:/a.sock", &panes).is_none());
        // two panes of one name: refused with the names to pick from
        assert_eq!(
            refusal(send("spg", &panes)).unwrap(),
            "2 panes are named \"spg\": spg#1024 (claude), spg#9906 (codex). Send again to one of them, or to claude:spg or codex:spg."
        );
        // a full name whose pane is gone can only have been a pane
        assert!(refusal(send("spg#4410", &panes)).unwrap().starts_with("no pane is named \"spg#4410\". The panes are: gds#2000 (claude)"));
        assert_eq!(refusal(send("kimi:gds#2000", &panes)).unwrap(), "gds#2000 runs claude, not kimi");
    }

    #[test]
    fn a_full_name_reaches_a_pane_that_changed_directory() {
        let panes = vec![pane("alt", 1024, "claude", "alt", 2)];
        let out = send("spg#1024", &panes).unwrap();
        assert_eq!(out.get("updatedInput").unwrap().str_at("to"), Some("uds:/tmp/cc-socks/2.sock"));
        assert_eq!(out.str_at("additionalContext"), Some("spg#1024 is now called alt#1024."));
    }

    #[test]
    fn the_context_maps_pane_names_to_listed_names() {
        let e = |pid: i32, name: &str, agent: Option<Agent>| Entry {
            pid,
            session_id: None,
            name: Some(name.into()),
            cwd: None,
            socket: Some(format!("/tmp/cc-socks/{pid}.sock")),
            status: Some("idle".into()),
            proc_start: None,
            started_at_ms: None,
            features: Vec::new(),
            ours: agent.is_some(),
            class: None,
            relay: None,
            owner: None,
            agent,
            agent_pid: None,
            spool: None,
        };
        let entries = vec![e(1, "gds", Some(Agent::Codex)), e(2, "inputx-6a", None), e(3, "marspot-03", None), e(4, "marspot#9906", Some(Agent::Kimi))];
        let panes = vec![pane("gds", 2000, "codex", "gds", 1), pane("inputx", 3000, "claude", "inputx", 2), pane("marspot", 1024, "claude", "marspot#1024", 3), pane("marspot", 9906, "kimi", "marspot#9906", 4)];
        let text = pane_context(&entries, &panes, Some(&entries[2]));
        assert!(text.contains("`name#code`") && text.contains("`kind:name`"), "{text}");
        assert!(text.contains("  gds  codex  idle  (gds#2000) → listed above under the same name\n"), "{text}");
        assert!(text.contains("  inputx  claude  idle  (inputx#3000) → listed above as \"inputx-6a\"\n"), "{text}");
        assert!(text.contains("  marspot#1024  claude  idle → this session\n"), "{text}");
        assert!(text.contains("  marspot#9906  kimi  idle → listed above under the same name\n"), "{text}");
    }
}
