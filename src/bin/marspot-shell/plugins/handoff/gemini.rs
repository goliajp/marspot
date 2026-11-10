//! Read a gemini session file into turns.
//!
//! The file is a log of changes, not a list of messages.  A message
//! record written again under the same id replaces the earlier one (a
//! reply is written when its text arrives and again when its tool calls
//! finish); `$rewindTo` drops a message and everything after it; `$patch`
//! rewrites or removes messages already written.  So the records are
//! replayed in order and the turns are read off what is left.
//!
//! Two kinds of `user` record are not the user talking: the session
//! context gemini opens with, and tool results, which are sent to the
//! model in the user's role and carry no text.

use std::path::Path;

use super::history;
use super::json::{self, Value};
use super::transcript::{Agent, Extract, Turn, MARKER};

enum Said {
    User(String),
    Model(String),
    /// Kept for its position only: info and error lines, tool results.
    Other,
}

/// The text of a message: its parts' `text`, without the model's
/// thoughts.  Content is a string, one part, or a list of parts.
fn text_of(content: &Value) -> String {
    let part = |p: &Value| match p {
        Value::Str(s) => Some(s.clone()),
        _ if p.get("thought").and_then(Value::bool) == Some(true) => None,
        _ => p.str_at("text").map(str::to_string),
    };
    let text = match content {
        Value::Arr(parts) => parts.iter().filter_map(part).collect::<Vec<_>>().join(""),
        one => part(one).unwrap_or_default(),
    };
    text.trim().to_string()
}

/// What the user typed is `displayContent` when gemini expanded the
/// message (an `@file` reference) before sending it.
fn said(record: &Value) -> Said {
    let text = record
        .get("displayContent")
        .map(text_of)
        .filter(|t| !t.is_empty())
        .or_else(|| record.get("content").map(text_of))
        .unwrap_or_default();
    match record.str_at("type") {
        Some("user") if !text.is_empty() && !text.starts_with("<session_context>") && !text.starts_with("<hook_context>") => {
            Said::User(text)
        }
        Some("gemini") if !text.is_empty() => Said::Model(text),
        _ => Said::Other,
    }
}

/// The messages standing after every record read so far, in order.
#[derive(Default)]
struct Log {
    messages: Vec<(String, Said)>,
}

impl Log {
    fn at(&self, id: &str) -> Option<usize> {
        self.messages.iter().position(|(m, _)| m == id)
    }

    fn patch(&mut self, patch: &Value) {
        let (Some(id), Some(content)) = (patch.str_at("id"), patch.get("content")) else { return };
        let Some(i) = self.at(id) else { return };
        let text = text_of(content);
        match &mut self.messages[i].1 {
            Said::User(t) | Said::Model(t) if !text.is_empty() => *t = text,
            _ => {}
        }
    }

    fn line(&mut self, line: &str) {
        let Ok(v) = json::parse(line) else { return };
        if let Some(id) = v.str_at("$rewindTo") {
            // an id from before where reading started takes everything read
            self.messages.truncate(self.at(id).unwrap_or(0));
        } else if let Some(p) = v.get("$patch") {
            self.patch(p);
            p.get("updates").map(Value::arr).unwrap_or(&[]).iter().for_each(|u| self.patch(u));
            for id in p.get("removeIds").map(Value::arr).unwrap_or(&[]).iter().filter_map(Value::str) {
                if let Some(i) = self.at(id) {
                    self.messages.remove(i);
                }
            }
        } else if let (Some(id), Some(_)) = (v.str_at("id"), v.str_at("type")) {
            let now = said(&v);
            match self.at(id) {
                Some(i) => self.messages[i].1 = now,
                None => self.messages.push((id.to_string(), now)),
            }
        }
    }

    /// A turn is an ask and the last thing the model said to it: what
    /// it says before its tool calls is narration, the text after the
    /// last of them is the answer.
    fn turns(self) -> Vec<Turn> {
        let mut turns: Vec<Turn> = Vec::new();
        let mut open = false;
        for (_, m) in self.messages {
            match m {
                Said::User(text) => {
                    turns.push(Turn { user: text, ..Default::default() });
                    open = true;
                }
                Said::Model(text) => {
                    if !open {
                        turns.push(Turn { user: "(continuing the previous request)".into(), ..Default::default() });
                        open = true;
                    }
                    turns.last_mut().expect("a turn is open").reply = text;
                }
                Said::Other => {}
            }
        }
        turns.retain(|t| !t.user.starts_with(MARKER));
        turns
    }
}

/// Read `path` from `watermark`.
pub fn read(path: &Path, watermark: u64) -> std::io::Result<Extract> {
    let mut log = Log::default();
    let end = history::for_each_line(path, watermark, |_, line| log.line(line))?;
    Ok(Extract {
        source: Agent::Gemini,
        session_id: crate::plugins::gemini::session_id_of(path).unwrap_or_default(),
        history_path: path.to_path_buf(),
        turns: log.turns(),
        end_offset: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::gemini::fixture::{context, file_name, head, reply, user};

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    fn write(name: &str, body: &str) -> std::path::PathBuf {
        let p = history::tmp(&format!("gemini-{name}")).join(file_name(ID));
        std::fs::write(&p, body).unwrap();
        p
    }

    fn asks(x: &Extract) -> Vec<&str> {
        x.turns.iter().map(|t| t.user.as_str()).collect()
    }

    #[test]
    fn a_turn_is_the_ask_and_the_text_after_the_last_tool_call() {
        // the narration, written again with its finished tool call
        let narration = r#"{"id":"g1","timestamp":"t","type":"gemini","content":"Looking around first.","thoughts":[],"model":"gemini-3-pro"}"#;
        let with_tool = r#"{"id":"g1","timestamp":"t","type":"gemini","content":"Looking around first.","thoughts":[],"model":"gemini-3-pro","toolCalls":[{"id":"t1","name":"run_shell_command","args":{"command":"npm test"},"status":"success","timestamp":"t"}]}"#;
        let tool_result = r#"{"id":"r1","timestamp":"t","type":"user","content":[{"functionResponse":{"id":"t1","name":"run_shell_command","response":{"output":"1 failed"}}}]}"#;
        let body = [
            head(ID),
            context(),
            user("u1", "make the demo"),
            format!("{narration}\n{with_tool}\n{tool_result}\n"),
            reply("g2", "Done; one test fails.", "gemini-3-pro"),
        ]
        .concat();
        let path = write("turn", &body);
        let x = read(&path, 0).unwrap();
        assert_eq!(x.source, Agent::Gemini);
        assert_eq!(x.session_id, ID);
        assert_eq!(x.turns, vec![Turn { user: "make the demo".into(), reply: "Done; one test fails.".into() }]);
        assert_eq!(x.end_offset, body.len() as u64);
    }

    #[test]
    fn a_rewind_takes_back_what_followed_it() {
        let body = [
            head(ID),
            user("u1", "first"),
            reply("g1", "one", "m"),
            user("u2", "a wrong turn"),
            reply("g2", "two", "m"),
            "{\"$rewindTo\":\"u2\"}\n".to_string(),
            user("u3", "second"),
            reply("g3", "three", "m"),
        ]
        .concat();
        let x = read(&write("rewind", &body), 0).unwrap();
        assert_eq!(asks(&x), vec!["first", "second"]);
        assert_eq!(x.turns[1].reply, "three");
    }

    /// The shape a resume appends: gemini rewrites a reply's content as
    /// parts.
    #[test]
    fn a_patch_rewrites_and_removes_messages() {
        let body = [
            head(ID),
            user("u1", "say hi"),
            reply("g1", "hi", "m"),
            user("u2", "dropped"),
            r#"{"$patch":{"updates":[{"id":"g1","content":[{"text":"hi there"}]}],"removeIds":["u2"]}}"#.to_string() + "\n",
        ]
        .concat();
        let x = read(&write("patch", &body), 0).unwrap();
        assert_eq!(x.turns, vec![Turn { user: "say hi".into(), reply: "hi there".into() }]);
    }

    #[test]
    fn a_handoff_turn_is_left_out_and_reading_resumes_at_the_watermark() {
        let first = [head(ID), user("u1", "[marspot handoff] This pane just moved here"), reply("g1", "Here is where things stand", "m")].concat();
        let body = [first.clone(), user("u2", "next"), reply("g2", "ok", "m")].concat();
        let path = write("marker", &body);
        assert_eq!(asks(&read(&path, 0).unwrap()), vec!["next"]);
        let later = read(&path, first.len() as u64).unwrap();
        assert_eq!(later.turns, vec![Turn { user: "next".into(), reply: "ok".into() }]);
    }

    #[test]
    fn what_the_user_typed_is_carried_not_what_gemini_expanded_it_to() {
        let expanded = r#"{"id":"u1","timestamp":"t","type":"user","content":[{"text":"read @a.rs"},{"text":"\n--- a.rs ---\nfn main() {}"}],"displayContent":[{"text":"read @a.rs"}]}"#;
        let thought = r#"{"id":"g1","timestamp":"t","type":"gemini","content":[{"text":"planning","thought":true},{"text":"It is a main function."}]}"#;
        let x = read(&write("display", &format!("{}{expanded}\n{thought}\n", head(ID))), 0).unwrap();
        assert_eq!(x.turns, vec![Turn { user: "read @a.rs".into(), reply: "It is a main function.".into() }]);
    }

    #[test]
    fn a_file_that_is_not_a_session_hands_nothing_over() {
        let x = read(&write("junk", "not json\n{\"id\":3}\n[1,2]\n"), 0).unwrap();
        assert!(x.turns.is_empty());
        assert_eq!(x.session_id, "");
        assert!(read(Path::new("/nonexistent/session.jsonl"), 0).is_err());
    }
}
