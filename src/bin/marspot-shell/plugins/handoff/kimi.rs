//! Read a kimi session record into turns.
//!
//! `agents/main/wire.jsonl` is a log of what the agent did, one record
//! per line.  A turn opens with `turn.prompt`, whose `input` is what
//! was submitted, and the model's words arrive as `content.part` events
//! inside the steps that follow.  Two things in a prompt are not the
//! user talking: what a hook put in front of it (`<hook_result …>`),
//! and prompts kimi raised itself, whose origin is not `user`.

use std::path::Path;

use super::history;
use super::json::{self, Value};
use super::transcript::{Agent, Extract, Turn, MARKER};

/// What the user typed in a `turn.prompt`.
fn typed(prompt: &Value) -> Option<String> {
    if prompt.at(&["origin", "kind"]).and_then(Value::str) != Some("user") {
        return None;
    }
    let text = prompt
        .get("input")
        .map_or(&[][..], Value::arr)
        .iter()
        .filter(|part| part.str_at("type") == Some("text"))
        .filter_map(|part| part.str_at("text"))
        .filter(|t| !t.trim_start().starts_with("<hook_result"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[derive(Default)]
struct Log {
    turns: Vec<Turn>,
    /// The text of the step being read.
    step: String,
}

impl Log {
    fn line(&mut self, line: &str) {
        // the two record types read here lead their lines
        let wanted = line.starts_with(r#"{"type":"turn.prompt""#) || line.starts_with(r#"{"type":"context.append_loop_event""#);
        let Some(v) = wanted.then(|| json::parse(line).ok()).flatten() else { return };
        if v.str_at("type") == Some("turn.prompt") {
            self.close_step();
            if let Some(user) = typed(&v) {
                self.turns.push(Turn { user, ..Default::default() });
            }
            return;
        }
        let Some(event) = v.get("event") else { return };
        match event.str_at("type") {
            Some("step.begin") => self.close_step(),
            Some("content.part") if event.at(&["part", "type"]).and_then(Value::str) == Some("text") => {
                self.step.push_str(event.at(&["part", "text"]).and_then(Value::str).unwrap_or_default());
            }
            _ => {}
        }
    }

    /// A turn's answer is what its last step said: what the model
    /// says before a tool call is narration.
    fn close_step(&mut self) {
        let said = std::mem::take(&mut self.step);
        let said = said.trim();
        if said.is_empty() {
            return;
        }
        if self.turns.is_empty() {
            // reading started inside a turn
            self.turns.push(Turn { user: "(continuing the previous request)".into(), ..Default::default() });
        }
        self.turns.last_mut().expect("a turn is open").reply = said.to_string();
    }
}

/// The session a record belongs to: `<session>/agents/main/wire.jsonl`.
fn session_of(path: &Path) -> Option<String> {
    Some(path.parent()?.parent()?.parent()?.file_name()?.to_str()?.to_string())
}

/// Read `path` from `watermark`.
pub fn read(path: &Path, watermark: u64) -> std::io::Result<Extract> {
    let mut log = Log::default();
    let end = history::for_each_line(path, watermark, |_, line| log.line(line))?;
    log.close_step();
    log.turns.retain(|t| !t.user.starts_with(MARKER));
    Ok(Extract {
        source: Agent::Kimi,
        session_id: session_of(path).unwrap_or_default(),
        history_path: path.to_path_buf(),
        turns: log.turns,
        end_offset: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;
    use crate::plugins::kimi::fixture::{session, CANCELLED_THEN_COMPLETED, ID, STOP_HOOK, TOOL_CALL};

    fn turns(wire: &str, from: u64) -> (Vec<(String, String)>, Extract) {
        let home = tmp("kimi-read");
        let path = session(&home, ID, wire, None);
        let x = read(&path, from).unwrap();
        (x.turns.iter().map(|t| (t.user.clone(), t.reply.clone())).collect(), x)
    }

    fn t(user: &str, reply: &str) -> (String, String) {
        (user.to_string(), reply.to_string())
    }

    #[test]
    fn what_a_hook_put_before_the_prompt_is_not_the_user() {
        let (got, x) = turns(CANCELLED_THEN_COMPLETED, 0);
        assert_eq!(got, vec![t("hello", ""), t("again", "mock reply done")], "the interrupted turn has no answer");
        assert_eq!(x.session_id, ID);
        assert_eq!(x.source, Agent::Kimi);
        assert_eq!(x.end_offset, CANCELLED_THEN_COMPLETED.len() as u64);
    }

    #[test]
    fn a_turn_continued_by_a_hook_is_one_turn_with_its_last_answer() {
        let (got, _) = turns(STOP_HOOK, 0);
        assert_eq!(got, vec![t("say hi", "mock reply done")]);
    }

    #[test]
    fn the_answer_is_what_followed_the_last_tool_call() {
        let (got, _) = turns(TOOL_CALL, 0);
        assert_eq!(got, vec![t("use the probe", "mock reply done")]);
    }

    #[test]
    fn reading_resumes_where_the_last_handoff_stopped() {
        let (_, first) = turns(STOP_HOOK, 0);
        let more = format!(
            "{STOP_HOOK}{}\n{}\n{}\n",
            r#"{"type":"turn.prompt","agentId":"main","input":[{"type":"text","text":"[marspot handoff] read the document"}],"origin":{"kind":"user"},"turnId":1,"time":1}"#,
            r#"{"type":"turn.prompt","agentId":"main","input":[{"type":"text","text":"next thing"}],"origin":{"kind":"user"},"turnId":2,"time":2}"#,
            r#"{"type":"context.append_loop_event","agentId":"main","event":{"type":"content.part","turnId":"2","step":1,"part":{"type":"text","text":"on it"}},"time":3}"#,
        );
        let (got, _) = turns(&more, first.end_offset);
        assert_eq!(got, vec![t("next thing", "on it")], "a handoff this pane received is not handed back");
        let raised = r#"{"type":"turn.prompt","agentId":"main","input":[{"type":"text","text":"scheduled"}],"origin":{"kind":"system_trigger","name":"cron"},"turnId":3,"time":4}"#;
        let (got, _) = turns(&format!("{raised}\n"), 0);
        assert!(got.is_empty(), "a prompt kimi raised itself is not the user's");
    }
}
