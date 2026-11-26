//! Turns and tasks out of Claude's records.
//!
//! A reply that calls tools is an `assistant` record per content block,
//! calls as `tool_use` blocks; results come back in a `user` record as
//! `tool_result` blocks.  A background command's result says
//! `Command running in background with ID: <task>`, a monitor's
//! `Monitor started (task <task>`.  A task's end reaches the session as
//! a `<task-notification>`, queued as a `queue-operation` and then
//! handed over in a `user` record.  A turn ends at a `system` record of
//! subtype `turn_duration` or `stop_hook_summary`.

use marspot_agent::types::{FoldSink, RecordFold, SubLast, TaskChange, TaskStart, TurnState};
use marspot_json::Value;

use super::text::record_time;

/// One notification: `ended`: it carries a status (a monitor's event
/// has none); `waiting`: a subagent stopped with background work of its
/// own, so it is not finished.
struct Note<'a> {
    task: &'a str,
    ended: bool,
    waiting: bool,
}

pub fn take(line: &str, v: Option<&Value>, st: &mut RecordFold, out: &mut dyn FoldSink) {
    const KINDS: [&str; 5] = ["\"assistant\"", "\"user\"", "turn_duration", "stop_hook_summary", "queue-operation"];
    if !KINDS.iter().any(|k| line.contains(k)) {
        return;
    }
    let Some(v) = v else { return };
    let Some(at) = record_time(v) else { return };
    match v.str_at("type") {
        Some("assistant") => {
            let Some(content) = v.at(&["message", "content"]) else { return };
            let end = v.at(&["message", "stop_reason"]).and_then(Value::str) == Some("end_turn");
            seen(st, at);
            assistant(content, end, at, st, out);
        }
        Some("user") => {
            let Some(content) = v.at(&["message", "content"]) else { return };
            seen(st, at);
            user(content, at, st, out);
        }
        Some("system") if matches!(v.str_at("subtype"), Some("turn_duration" | "stop_hook_summary")) => {
            seen(st, at);
            st.turn = TurnState::Ended { at };
        }
        Some("queue-operation") if v.str_at("operation") == Some("enqueue") => {
            seen(st, at);
            notes(notes_in(v.str_at("content").unwrap_or("")), at, st, out);
        }
        _ => {}
    }
}

fn seen(st: &mut RecordFold, at: u64) {
    st.tasks.first.get_or_insert(at);
    st.tasks.last_seen = st.tasks.last_seen.max(at);
}

fn assistant(content: &Value, end: bool, at: u64, st: &mut RecordFold, out: &mut dyn FoldSink) {
    let mut uses = 0usize;
    let mut bash = None;
    for b in content.arr() {
        if b.str_at("type") != Some("tool_use") {
            continue;
        }
        let (Some(tool), Some(id)) = (b.str_at("name"), b.str_at("id")) else { continue };
        let field = |k: &str| b.get("input").and_then(|i| i.get(k));
        let background = tool == "Monitor" || field("run_in_background").and_then(Value::bool) == Some(true);
        let command = field("command").and_then(Value::str);
        out.task(TaskChange::Called { call: id, tool, background, command });
        st.tasks.called(id, background);
        uses += 1;
        if tool == "Bash" && !background {
            bash = Some(field("timeout").and_then(Value::i64).and_then(|t| u64::try_from(t).ok()));
        }
    }
    st.turn = TurnState::Working;
    st.tasks.last = match bash {
        Some(timeout_ms) => SubLast::Bash { at, timeout_ms },
        None if end && uses == 0 && st.tasks.open_calls() == 0 => SubLast::Idle,
        None => SubLast::Busy,
    };
    st.tasks.last_active = at;
}

fn user(content: &Value, at: u64, st: &mut RecordFold, out: &mut dyn FoldSink) {
    let mut found = notes_in(content.str().unwrap_or(""));
    for b in content.arr() {
        match b.str_at("type") {
            Some("tool_result") => {
                let Some(call) = b.str_at("tool_use_id") else { continue };
                let text = text_of(b.get("content"));
                let task = started_task(&text);
                let open = st.tasks.answered(call);
                if let Some(task) = task {
                    let how = TaskStart { known_call: open.is_some(), background: open.unwrap_or(true) };
                    st.tasks.started(task, how, true);
                }
                out.task(TaskChange::Answered { call, task, known: open.is_some(), background: open.unwrap_or(true) });
            }
            Some("text") => found.extend(notes_in(b.str_at("text").unwrap_or(""))),
            _ => {}
        }
    }
    notes(found, at, st, out);
    st.turn = TurnState::Working;
    st.tasks.last = SubLast::Busy;
    st.tasks.last_active = at;
}

fn notes(found: Vec<Note<'_>>, at: u64, st: &mut RecordFold, out: &mut dyn FoldSink) {
    for n in found.iter().filter(|n| n.ended) {
        st.tasks.ended(n.task, at, n.waiting);
        out.task(TaskChange::Ended { task: n.task, at, waiting: n.waiting });
    }
}

fn text_of(v: Option<&Value>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.str() {
        return s.to_string();
    }
    v.arr().iter().filter_map(|b| b.str_at("text")).collect()
}

/// The task a tool result says it started.
fn started_task(text: &str) -> Option<&str> {
    const MARKS: [&str; 2] = ["Command running in background with ID: ", "Monitor started (task "];
    MARKS.iter().find_map(|m| {
        let rest = &text[text.find(m)? + m.len()..];
        let n = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
        (n > 0).then_some(&rest[..n])
    })
}

fn notes_in(text: &str) -> Vec<Note<'_>> {
    let mut found = Vec::new();
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
            found.push(Note { task: task.trim(), ended, waiting });
            ids = &ids[ids.find("</task-id>").map_or(ids.len(), |e| e + "</task-id>".len())..];
        }
        rest = &body[end..];
    }
    found
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let from = s.find(&open)? + open.len();
    let to = s[from..].find(&close)? + from;
    Some(&s[from..to])
}
