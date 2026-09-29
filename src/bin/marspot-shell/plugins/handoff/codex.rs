//! Read a codex rollout into turns.
//!
//! The rollout records each turn twice: as model-facing
//! `response_item`s (reasoning encrypted, tool calls as JavaScript for
//! its `exec` tool) and as `item_completed` events, which are typed.
//! The handoff needs only two of those types — `UserMessage` and
//! `AgentMessage` — and lines can be megabytes (an image generation
//! carries the image), so the type is read off the raw line and only
//! those two are parsed.

use std::path::Path;

use super::history;
use super::json::{self, Value};
use super::transcript::{Agent, Extract, Turn, MARKER};

/// The item type of an `item_completed` line, without parsing it.
fn item_type(line: &str) -> Option<&str> {
    if !line.contains(r#""type":"item_completed""#) {
        return None;
    }
    let key = r#""item":{"type":""#;
    let rest = &line[line.find(key)? + key.len()..];
    Some(&rest[..rest.find('"')?])
}

/// Read `path` from `watermark` (or its last compaction, if later).
pub fn read(path: &Path, watermark: u64) -> std::io::Result<Extract> {
    let start = history::last_line_with(path, br#""type":"compacted""#, watermark)?
        .unwrap_or(watermark);
    let mut b = Builder::default();
    let end = history::for_each_line(path, start, |_, line| {
        if !matches!(item_type(line), Some("UserMessage" | "AgentMessage")) {
            return;
        }
        let Ok(v) = json::parse(line) else { return };
        if let Some(item) = v.at(&["payload", "item"]) {
            b.item(item);
        }
    })?;
    Ok(Extract {
        source: Agent::Codex,
        session_id: history::codex_id_of(path).unwrap_or_default(),
        history_path: path.to_path_buf(),
        turns: b.finish(),
        end_offset: end,
    })
}

fn text_of(item: &Value, sep: &str) -> String {
    item.get("content")
        .map(Value::arr)
        .unwrap_or(&[])
        .iter()
        .filter_map(|c| c.str_at("text"))
        .collect::<Vec<_>>()
        .join(sep)
        .trim()
        .to_string()
}

#[derive(Default)]
struct Builder {
    turns: Vec<Turn>,
    cur: Option<(Turn, bool)>,
    /// A turn's reply is its final answer; the running commentary is
    /// only used when a turn ended without one.
    commentary: Option<String>,
}

impl Builder {
    fn close(&mut self) {
        if let Some((mut t, skip)) = self.cur.take() {
            if t.reply.is_empty() {
                t.reply = self.commentary.take().unwrap_or_default();
            }
            if !skip {
                self.turns.push(t);
            }
        }
        self.commentary = None;
    }

    fn item(&mut self, item: &Value) {
        if item.str_at("type") == Some("UserMessage") {
            self.close();
            let text = text_of(item, "\n");
            let skip = text.starts_with(MARKER);
            self.cur = Some((Turn { user: text, ..Default::default() }, skip));
            return;
        }
        let text = text_of(item, "");
        let (t, _) = self.cur.get_or_insert_with(|| {
            (Turn { user: "(continuing the previous request)".into(), ..Default::default() }, false)
        });
        if item.str_at("phase") == Some("final_answer") {
            if !t.reply.is_empty() {
                t.reply.push_str("\n\n");
            }
            t.reply.push_str(&text);
        } else {
            self.commentary = Some(text);
        }
    }

    fn finish(mut self) -> Vec<Turn> {
        self.close();
        self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(item: &str) -> String {
        format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"item_completed\",\"item\":{item}}}}}\n")
    }
    fn user(t: &str) -> String {
        ev(&format!(r#"{{"type":"UserMessage","id":"u","content":[{{"type":"text","text":"{t}"}}]}}"#))
    }
    fn agent(t: &str, phase: &str) -> String {
        ev(&format!(r#"{{"type":"AgentMessage","content":[{{"type":"Text","text":"{t}"}}],"phase":"{phase}"}}"#))
    }

    fn write(name: &str, s: &str) -> std::path::PathBuf {
        let p = history::tmp(&format!("codex-{name}"))
            .join("rollout-2026-09-20T11-27-08-01a0bca3-87a2-7aa2-8d42-d84a71b8539a.jsonl");
        std::fs::write(&p, s).unwrap();
        p
    }

    #[test]
    fn a_turn_is_the_ask_and_the_final_answer() {
        let s = [
            user("make the demo"),
            agent("Looking around first.", "commentary"),
            ev(r#"{"type":"CommandExecution","command":["/bin/zsh","-lc","npm test"],"exit_code":1}"#),
            ev(r#"{"type":"Reasoning","summary":[]}"#),
            agent("Done; one test fails.", "final_answer"),
        ]
        .concat();
        let x = read(&write("turn", &s), 0).unwrap();
        assert_eq!(x.session_id, "01a0bca3-87a2-7aa2-8d42-d84a71b8539a");
        assert_eq!(x.turns, vec![Turn { user: "make the demo".into(), reply: "Done; one test fails.".into() }]);
    }

    #[test]
    fn a_turn_without_a_final_answer_keeps_its_last_commentary() {
        let s = [user("go"), agent("halfway there", "commentary")].concat();
        let x = read(&write("commentary", &s), 0).unwrap();
        assert_eq!(x.turns[0].reply, "halfway there");
    }

    #[test]
    fn a_handoff_turn_is_left_out() {
        let s = [
            user("[marspot handoff] This pane just moved here"),
            agent("Here is where things stand", "final_answer"),
            user("next"),
            agent("ok", "final_answer"),
        ]
        .concat();
        let x = read(&write("marker", &s), 0).unwrap();
        assert_eq!(x.turns.iter().map(|t| t.user.as_str()).collect::<Vec<_>>(), vec!["next"]);
    }

    /// Reading starts at the last compaction: what came before it the
    /// agent itself no longer had.
    #[test]
    fn reading_starts_at_the_last_compaction() {
        let s = format!(
            "{}{}{}{}{}",
            user("ancient"),
            agent("x", "final_answer"),
            "{\"type\":\"compacted\",\"payload\":{\"message\":\"\",\"replacement_history\":[]}}\n",
            user("after"),
            agent("fine", "final_answer")
        );
        let x = read(&write("compact", &s), 0).unwrap();
        assert_eq!(x.turns.iter().map(|t| t.user.as_str()).collect::<Vec<_>>(), vec!["after"]);
    }
}
