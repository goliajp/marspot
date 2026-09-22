//! Read a claude transcript into turns.
//!
//! The file is a TREE, not a list: every record names its parent, and a
//! rewind or an edited message leaves the abandoned branch in the file.
//! Reading in file order would hand the next agent a branch the user
//! backed out of.  So the records are indexed by uuid and the handoff
//! is the chain from the newest record back to where reading started.

use std::collections::HashMap;
use std::path::Path;

use super::history;
use super::json::{self, Value};
use super::transcript::{Agent, Extract, Turn, MARKER};

/// What one record contributes to a turn.  The parsed JSON is dropped
/// as soon as this is taken from it — a record can be megabytes of
/// tool output.
enum Piece {
    /// A message the user typed.
    Ask(String),
    Text(String),
    /// A tool call.  Only its position matters: the reply is the text
    /// after the last one, not the narration before it.
    Tool,
}

struct Node {
    parent: Option<String>,
    pieces: Vec<Piece>,
}

/// Read `path` from `watermark` (or its last compaction, if later —
/// what came before a compaction the agent itself no longer had).
pub fn read(path: &Path, session_id: &str, watermark: u64) -> std::io::Result<Extract> {
    let start = history::last_line_with(path, br#""subtype":"compact_boundary""#, watermark)?
        .unwrap_or(watermark);
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut leaf: Option<String> = None;
    let end = history::for_each_line(path, start, |_, line| {
        let Ok(v) = json::parse(line) else { return };
        let Some(uuid) = v.str_at("uuid") else { return };
        if v.get("isSidechain").and_then(Value::bool) == Some(true) {
            return; // a subagent's own conversation
        }
        let kind = v.str_at("type").unwrap_or("");
        if kind == "user" || kind == "assistant" {
            leaf = Some(uuid.to_string());
        }
        nodes.insert(
            uuid.to_string(),
            Node { parent: v.str_at("parentUuid").map(str::to_string), pieces: pieces_of(&v) },
        );
    })?;

    // Walk from the newest message back to where reading started.
    let mut chain: Vec<Node> = Vec::new();
    let mut at = leaf;
    while let Some(id) = at {
        let Some(node) = nodes.remove(&id) else { break };
        at = node.parent.clone();
        chain.push(node);
    }
    chain.reverse();
    Ok(Extract {
        source: Agent::Claude,
        session_id: session_id.to_string(),
        history_path: path.to_path_buf(),
        turns: build_turns(chain.into_iter().flat_map(|n| n.pieces)),
        end_offset: end,
    })
}

fn pieces_of(v: &Value) -> Vec<Piece> {
    let content = v.at(&["message", "content"]);
    match v.str_at("type").unwrap_or("") {
        "user" => {
            // Summaries claude wrote for itself, skill bodies, injected
            // context: none of it is the user talking.
            let own = |k: &str| v.get(k).and_then(Value::bool) == Some(true);
            if own("isCompactSummary") || own("isMeta") {
                return Vec::new();
            }
            // Written into the user's slot by the harness: a background
            // task reporting in (`origin.kind = task-notification`), a
            // slash command's output.  The agent answers these, so they
            // bound the reply the way a tool call does.
            let text = text_of(content);
            let not_human = v.at(&["origin", "kind"]).and_then(Value::str).is_some_and(|k| k != "human");
            if not_human || text.trim_start().starts_with("<local-command-stdout>") {
                return vec![Piece::Tool];
            }
            let ask = strip_reminders(&text);
            if ask.is_empty() { Vec::new() } else { vec![Piece::Ask(ask)] }
        }
        "assistant" => content
            .map(Value::arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(|b| match b.str_at("type")? {
                "text" => Some(Piece::Text(b.str_at("text")?.to_string())),
                "tool_use" => Some(Piece::Tool),
                _ => None, // thinking: signed, and not the next agent's
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A user record's typed text: a plain string, or its `text` blocks.
/// `tool_result` blocks are claude's own tool output, not the user.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Arr(blocks)) => blocks
            .iter()
            .filter(|b| b.str_at("type") == Some("text"))
            .filter_map(|b| b.str_at("text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `<system-reminder>` blocks are the harness talking, not the user.
fn strip_reminders(s: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find(CLOSE) {
            Some(j) => &rest[i + j + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Fold the chain's pieces into turns.  A turn opened by a handoff
/// message is dropped whole — that content came from the other side.
fn build_turns(pieces: impl Iterator<Item = Piece>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut cur: Option<(Turn, bool)> = None;
    let mut after_tool: Vec<String> = Vec::new();
    let mut close = |cur: &mut Option<(Turn, bool)>, after: &mut Vec<String>| {
        if let Some((mut t, skip)) = cur.take() {
            t.reply = after.join("\n\n");
            if !skip {
                turns.push(t);
            }
        }
        after.clear();
    };
    for p in pieces {
        match p {
            Piece::Ask(text) => {
                close(&mut cur, &mut after_tool);
                let skip = text.starts_with(MARKER);
                cur = Some((Turn { user: text, ..Default::default() }, skip));
            }
            Piece::Text(t) => {
                cur.get_or_insert_with(continued);
                after_tool.push(t);
            }
            Piece::Tool => {
                cur.get_or_insert_with(continued);
                after_tool.clear();
            }
        }
    }
    close(&mut cur, &mut after_tool);
    turns
}

/// A turn whose user message is before the watermark.
fn continued() -> (Turn, bool) {
    (Turn { user: "(continuing the previous request)".into(), ..Default::default() }, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(uuid: &str, parent: Option<&str>, body: &str) -> String {
        let p = parent.map(|p| format!("\"{p}\"")).unwrap_or("null".into());
        format!("{{\"uuid\":\"{uuid}\",\"parentUuid\":{p},\"isSidechain\":false,{body}}}\n")
    }
    fn user(uuid: &str, parent: Option<&str>, text: &str) -> String {
        rec(uuid, parent, &format!(r#""type":"user","message":{{"role":"user","content":"{text}"}}"#))
    }
    fn say(uuid: &str, parent: &str, text: &str) -> String {
        rec(uuid, Some(parent), &format!(
            r#""type":"assistant","message":{{"content":[{{"type":"thinking","thinking":"secret","signature":"x"}},{{"type":"text","text":"{text}"}}]}}"#
        ))
    }
    fn tool(uuid: &str, parent: &str) -> String {
        rec(uuid, Some(parent), r#""type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}"#)
    }
    fn result(uuid: &str, parent: &str) -> String {
        rec(uuid, Some(parent), r#""type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}"#)
    }

    fn write(name: &str, s: &str) -> std::path::PathBuf {
        let p = history::tmp(&format!("claude-{name}")).join("s.jsonl");
        std::fs::write(&p, s).unwrap();
        p
    }

    #[test]
    fn a_turn_is_the_ask_and_the_final_reply() {
        let s = [
            user("a", None, "run the tests"),
            say("b", "a", "I will run them."),
            tool("c", "b"),
            result("d", "c"),
            say("e", "d", "Two failures, both in parser."),
        ]
        .concat();
        let x = read(&write("turn", &s), "S", 0).unwrap();
        assert_eq!(
            x.turns,
            vec![Turn { user: "run the tests".into(), reply: "Two failures, both in parser.".into() }],
            "the narration before the tools is not the reply; tool output is not the user"
        );
        assert_eq!(x.end_offset, s.len() as u64);
    }

    /// A rewind leaves the abandoned branch in the file; the handoff
    /// follows the branch the user kept.
    #[test]
    fn an_abandoned_branch_is_not_handed_over() {
        let s = [
            user("a", None, "first"),
            say("b", "a", "ok"),
            user("c", Some("b"), "the path I backed out of"),
            say("d", "c", "went wrong"),
            user("e", Some("b"), "the path I kept"),
            say("f", "e", "right"),
        ]
        .concat();
        let x = read(&write("branch", &s), "S", 0).unwrap();
        let asks: Vec<&str> = x.turns.iter().map(|t| t.user.as_str()).collect();
        assert_eq!(asks, vec!["first", "the path I kept"]);
    }

    /// The previous handoff INTO claude, and claude's restatement of it,
    /// are the other side's content — leaving them out is what stops
    /// A→B→A from nesting.
    #[test]
    fn a_handoff_turn_is_left_out() {
        let s = [
            user("a", None, "[marspot handoff] This pane just moved here"),
            say("b", "a", "Here is where things stand"),
            user("c", Some("b"), "now do the next thing"),
            say("d", "c", "done"),
        ]
        .concat();
        let x = read(&write("marker", &s), "S", 0).unwrap();
        assert_eq!(x.turns.len(), 1);
        assert_eq!(x.turns[0].user, "now do the next thing");
    }

    /// Reading starts at the watermark, or the last compaction after it.
    #[test]
    fn reading_starts_at_the_later_of_watermark_and_compaction() {
        let before = [user("a", None, "old ask"), say("b", "a", "old reply")].concat();
        let after = [
            rec("k", None, r#""type":"system","subtype":"compact_boundary","content":"Conversation compacted""#),
            rec("l", Some("k"), r#""type":"user","isCompactSummary":true,"message":{"content":"SUMMARY OF OLD"}"#),
            user("m", Some("l"), "new ask"),
            say("n", "m", "new reply"),
        ]
        .concat();
        let p = write("compact", &format!("{before}{after}"));
        let x = read(&p, "S", 0).unwrap();
        assert_eq!(x.turns.iter().map(|t| t.user.as_str()).collect::<Vec<_>>(), vec!["new ask"]);

        let wm = (before.len() + after.len()) as u64;
        std::fs::write(&p, format!("{before}{after}{}{}", user("o", Some("n"), "newest"), say("q", "o", "r"))).unwrap();
        let x = read(&p, "S", wm).unwrap();
        assert_eq!(x.turns.iter().map(|t| t.user.as_str()).collect::<Vec<_>>(), vec!["newest"]);
    }

    /// A background task reporting in is not the user; what the agent
    /// said after it is the latest word.
    #[test]
    fn a_task_notification_is_not_the_user() {
        let s = [
            user("a", None, "review it"),
            say("b", "a", "started a reviewer"),
            rec("c", Some("b"), r#""type":"user","origin":{"kind":"task-notification"},"message":{"content":"<task-notification>done</task-notification>"}"#),
            say("d", "c", "the reviewer found two issues"),
        ]
        .concat();
        let x = read(&write("notify", &s), "S", 0).unwrap();
        assert_eq!(x.turns, vec![Turn { user: "review it".into(), reply: "the reviewer found two issues".into() }]);
    }

    #[test]
    fn reminders_and_meta_records_are_not_the_user() {
        let s = [
            user("a", None, "<system-reminder>noise</system-reminder>real ask"),
            rec("b", Some("a"), r#""type":"user","isMeta":true,"message":{"content":"skill body"}"#),
            say("c", "b", "ok"),
        ]
        .concat();
        let x = read(&write("meta", &s), "S", 0).unwrap();
        assert_eq!(x.turns, vec![Turn { user: "real ask".into(), reply: "ok".into() }]);
    }
}
