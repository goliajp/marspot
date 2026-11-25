//! One line of a claude transcript, reduced to what the rules read.
//!
//! Shapes taken from live transcripts (claude 2.1.33x): a reply that
//! calls tools is an `assistant` record per content block, tool calls
//! as `tool_use` blocks; their results come back in a `user` record as
//! `tool_result` blocks; a background command's result says
//! `Command running in background with ID: <task>`, a monitor's
//! `Monitor started (task <task>`; a task's end reaches the session as
//! a `<task-notification>` (queued as a `queue-operation`, then handed
//! over in a `user` record); the end of a turn is a `system` record of
//! subtype `turn_duration` or `stop_hook_summary`.  Everything else
//! (bookkeeping, api requests, attachments) is skipped.

use marspot_peer::json::{self, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    /// Runs past the turn: a `Bash` with `run_in_background`, or any `Monitor`.
    pub background: bool,
    pub command: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub task: String,
    /// It carries a status: the task ended.  A monitor's event has none.
    pub ended: bool,
    /// A subagent stopped with background work of its own: it is not
    /// finished, it waits for that work.
    pub waiting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rec {
    /// `end`: the reply stops there (`stop_reason` `end_turn`).
    Assistant { at: u64, uses: Vec<ToolUse>, end: bool },
    /// Tool results (call id, the task it started) and notifications;
    /// both empty for a prompt.
    User { at: u64, results: Vec<(String, Option<String>)>, notes: Vec<Note> },
    TurnEnd { at: u64 },
    Queued { at: u64, notes: Vec<Note> },
}

impl Rec {
    pub fn at(&self) -> u64 {
        match self {
            Rec::Assistant { at, .. } | Rec::User { at, .. } | Rec::TurnEnd { at } | Rec::Queued { at, .. } => *at,
        }
    }
}

/// The record on `line`, or None for one the rules do not read.
pub fn parse(line: &str) -> Option<Rec> {
    // most lines are skipped; the parse is only paid for these
    const KINDS: [&str; 5] = ["\"assistant\"", "\"user\"", "turn_duration", "stop_hook_summary", "queue-operation"];
    if !KINDS.iter().any(|k| line.contains(k)) {
        return None;
    }
    let v = json::parse(line).ok()?;
    let at = crate::cc_usage::parse_iso_utc(v.str_at("timestamp")?.to_string())?;
    let at = u64::try_from(at).ok()?;
    match v.str_at("type")? {
        "assistant" => {
            let content = v.at(&["message", "content"])?;
            let end = v.at(&["message", "stop_reason"]).and_then(Value::str) == Some("end_turn");
            Some(Rec::Assistant { at, uses: content.arr().iter().filter_map(tool_use).collect(), end })
        }
        "user" => {
            let content = v.at(&["message", "content"])?;
            let mut results = Vec::new();
            let mut notes = notes_in(content.str().unwrap_or(""));
            for b in content.arr() {
                match b.str_at("type") {
                    Some("tool_result") => {
                        let Some(id) = b.str_at("tool_use_id") else { continue };
                        results.push((id.to_string(), started_task(&text_of(b.get("content")))));
                    }
                    Some("text") => notes.extend(notes_in(b.str_at("text").unwrap_or(""))),
                    _ => {}
                }
            }
            Some(Rec::User { at, results, notes })
        }
        "system" => matches!(v.str_at("subtype")?, "turn_duration" | "stop_hook_summary").then_some(Rec::TurnEnd { at }),
        "queue-operation" if v.str_at("operation") == Some("enqueue") => {
            Some(Rec::Queued { at, notes: notes_in(v.str_at("content").unwrap_or("")) })
        }
        _ => None,
    }
}

fn tool_use(b: &Value) -> Option<ToolUse> {
    if b.str_at("type")? != "tool_use" {
        return None;
    }
    let name = b.str_at("name")?.to_string();
    let input = b.get("input");
    let field = |k: &str| input.and_then(|i| i.get(k));
    let background = name == "Monitor" || field("run_in_background").and_then(Value::bool) == Some(true);
    Some(ToolUse {
        id: b.str_at("id")?.to_string(),
        command: field("command").and_then(Value::str).map(str::to_string),
        timeout_ms: field("timeout").and_then(Value::i64).and_then(|t| u64::try_from(t).ok()),
        background,
        name,
    })
}

fn text_of(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.str() {
        return s.to_string();
    }
    v.arr().iter().filter_map(|b| b.str_at("text")).collect()
}

/// The task a tool result says it started.
fn started_task(text: &str) -> Option<String> {
    const MARKS: [&str; 2] = ["Command running in background with ID: ", "Monitor started (task "];
    MARKS.iter().find_map(|m| {
        let rest = &text[text.find(m)? + m.len()..];
        let id: String = rest.chars().take_while(char::is_ascii_alphanumeric).collect();
        (!id.is_empty()).then_some(id)
    })
}

fn notes_in(text: &str) -> Vec<Note> {
    let mut notes = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<task-notification>") {
        let body = &rest[i..];
        let end = body.find("</task-notification>").map_or(body.len(), |e| e + "</task-notification>".len());
        let n = &body[..end];
        let ended = tag(n, "status").is_some_and(|s| !s.trim().is_empty());
        let waiting = tag(n, "summary").is_some_and(|s| s.contains("waiting on its own background work"));
        // one notification can name several tasks: the agents a previous process left running
        let mut ids = n;
        while let Some(task) = tag(ids, "task-id") {
            notes.push(Note { task: task.trim().to_string(), ended, waiting });
            ids = &ids[ids.find("</task-id>").map_or(ids.len(), |e| e + "</task-id>".len())..];
        }
        rest = &body[end..];
    }
    notes
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = s.find(&open)? + open.len();
    let to = s[from..].find(&close)? + from;
    Some(&s[from..to])
}

#[cfg(test)]
mod tests;
