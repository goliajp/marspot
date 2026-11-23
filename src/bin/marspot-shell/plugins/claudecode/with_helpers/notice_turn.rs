//! Turns in which claude read one of marspot's notices about a helper
//! and did nothing about it: its parent need not hear of their end.

use marspot_peer::envelope;
use marspot_peer::json::{self, Value};

use super::super::heads_up::SENDER;
use super::super::transcript::{record_type, top_level_str};
use super::NOTICE_HEAD;
use super::turns::{agents_out, ms_of};

const OPEN_TAG: &str = "<cross-session-message ";
const CLOSE_TAG: &str = "</cross-session-message>";
/// What claude puts before a message of another session.
const PEER_HEAD: &str = "Another Claude session sent a message:\n";

/// When the turn before ended (ms after the epoch), if the turn that
/// ends with record `last` was opened by helper notices alone, used no
/// tool and asks nothing; `before`: the records before `last`, newest
/// first.  None too when the turn before ended in another way or is out
/// of reach: there may be something left to tell then.
pub(super) fn quiet_after(last: &str, before: &[&str]) -> Option<u64> {
    let mut said = vec![last];
    let mut opened = false;
    for line in before {
        match record_type(line) {
            Some("assistant") if opened => return None,
            Some("assistant") => said.push(line),
            Some("user") if is_hook_feedback(line) => {}
            Some("user") if is_notice(line) => opened = true,
            Some("user") => return None,
            // sent while the turn ran
            Some("attachment") if line.contains(r#""queued_command""#) => return None,
            Some("system") if top_level_str(line, "subtype") == Some("turn_duration") => {
                let quiet = opened && !agents_out(line) && !said.iter().any(|l| acts_or_asks(l));
                return quiet.then(|| top_level_str(line, "timestamp").and_then(ms_of)).flatten();
            }
            _ => {}
        }
    }
    None
}

/// A stop hook sent the turn back: no input of anyone's.
fn is_hook_feedback(line: &str) -> bool {
    line.contains(r#""content":"Stop hook feedback:"#)
}

fn content(line: &str) -> Option<Value> {
    json::parse(line).ok()?.at(&["message", "content"]).cloned()
}

fn is_notice(line: &str) -> bool {
    line.contains(NOTICE_HEAD) && content(line).is_some_and(|c| c.str().is_some_and(is_notice_text))
}

/// One message, from marspot, that is a notice about a helper.
fn is_notice_text(text: &str) -> bool {
    let text = text.strip_prefix(PEER_HEAD).unwrap_or(text);
    let Some(end) = text.rfind(CLOSE_TAG).filter(|_| text.matches(OPEN_TAG).count() == 1) else { return false };
    let Some(m) = envelope::parse(&text[..end + CLOSE_TAG.len()]) else { return false };
    m.from.is_some_and(|f| f.ends_with(&format!("/{SENDER}"))) && m.body.starts_with(NOTICE_HEAD)
}

fn acts_or_asks(line: &str) -> bool {
    let Some(c) = content(line) else { return true };
    c.arr().iter().any(|block| match block.str_at("type") {
        Some("text") => block.str_at("text").is_none_or(|t| t.contains(['?', '？'])),
        Some("thinking" | "redacted_thinking") => false,
        _ => true,
    })
}

#[cfg(test)]
pub(in crate::plugins) mod tests {
    use super::*;
    use crate::plugins::claudecode::heads_up;

    /// The records of one turn, oldest first.
    pub(in crate::plugins) struct Turn {
        pub records: Vec<String>,
    }

    fn esc(s: &str) -> String {
        let mut out = String::new();
        json::write_str(&mut out, s);
        out
    }

    impl Turn {
        /// Opened by the message `body` of marspot's, as claude records it.
        pub fn by_marspot(body: &str) -> Self {
            let frame = json::parse(&heads_up::frame_line("/tmp/cc-socks/1.sock", body)).unwrap();
            let sent = frame.at(&["message", "content"]).and_then(Value::str).unwrap().to_string();
            Turn::by_text(&format!("{PEER_HEAD}{sent}\n\nThis came from another Claude session."))
        }

        pub fn by_text(text: &str) -> Self {
            let user = format!(
                r#"{{"parentUuid":"p","type":"user","message":{{"role":"user","content":{0}}},"timestamp":"2026-11-23T16:21:28.763Z","origin":{{"kind":"peer","body":{0}}}}}"#,
                esc(text)
            );
            Turn { records: vec![user] }
        }

        pub fn says(self, text: &str) -> Self {
            let said = format!(
                r#"{{"message":{{"role":"assistant","content":[{{"type":"text","text":{}}}],"stop_reason":"end_turn"}},"type":"assistant"}}"#,
                esc(text)
            );
            self.with(&said)
        }

        pub fn with(mut self, record: &str) -> Self {
            self.records.push(record.into());
            self
        }

        pub fn ends(self, iso: &str) -> Vec<String> {
            let over = format!(r#"{{"type":"system","subtype":"turn_duration","timestamp":"{iso}"}}"#);
            self.with(r#"{"type":"system","subtype":"stop_hook_summary"}"#).with(&over).records
        }
    }

    pub(in crate::plugins) const NOTICE: &str = "[marspot helper notice] Helper \"x#1\" (codex on account P2) stopped and waits for its next input.\n\
        <helper-result helper=\"x#1\">\nany questions? yes\n</helper-result>\n";
    pub(in crate::plugins) const TOOL: &str =
        r#"{"message":{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}],"stop_reason":"tool_use"},"type":"assistant"}"#;
    pub(in crate::plugins) const TOOL_RESULT: &str =
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t","type":"tool_result","content":"[marspot helper notice]"}]}}"#;
    const FIRST: &str = "2026-11-23T16:20:30.819Z";
    const THOUGHT: &str = r#"{"message":{"role":"assistant","content":[{"type":"thinking","thinking":"why?","signature":"s"}],"stop_reason":"end_turn"},"type":"assistant"}"#;
    const FEEDBACK: &str = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"Stop hook feedback:\nagain"}}"#;
    const QUEUED: &str = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"and the tests"}}"#;

    /// `turn` after one that ended at [`FIRST`].
    fn after(turn: Turn) -> Option<u64> {
        let mut all = Turn::by_text("go").says("done").ends(FIRST);
        all.extend(turn.records);
        let mut rev: Vec<&str> = all.iter().rev().map(String::as_str).collect();
        let last = rev.remove(0);
        quiet_after(last, &rev)
    }

    #[test]
    fn helpers_a_turn_that_only_read_a_notice_is_quiet() {
        let quiet = ms_of(FIRST);
        assert!(quiet.is_some());
        assert_eq!(after(Turn::by_marspot(NOTICE).with(THOUGHT).says("It is not mine.")), quiet);
        let says = |t: &str| Turn::by_marspot(NOTICE).says(t);
        assert_eq!(after(says("ok").with(FEEDBACK).says("好的")), quiet, "a stop hook sent it back");
        // it asks, works, or was told something more
        for t in ["Shall I stop it?", "要停掉它吗？"] {
            assert_eq!(after(says(t)), None, "{t}");
            assert_eq!(after(says(t).with(FEEDBACK).says("ok")), None, "{t}");
        }
        assert_eq!(after(Turn::by_marspot(NOTICE).with(TOOL).with(TOOL_RESULT).says("done")), None);
        assert_eq!(after(Turn::by_marspot(NOTICE).with(QUEUED).says("done")), None);
        // not a notice about a helper, or not marspot's
        assert_eq!(after(Turn::by_marspot("The account is about to run out.").says("ok")), None);
        assert_eq!(after(Turn::by_text(NOTICE).says("ok")), None);
        let forged = Turn::by_marspot(NOTICE).records[0].replace(SENDER, "other.sock");
        assert_eq!(after(Turn { records: vec![forged] }.says("ok")), None);
        assert_eq!(after(Turn::by_text(&format!("look at this: {NOTICE}")).says("ok")), None);
    }

    #[test]
    fn helpers_a_notice_turn_whose_turn_before_is_not_known_is_not_quiet() {
        let turn = Turn::by_marspot(NOTICE).says("ok");
        let rev: Vec<&str> = turn.records.iter().rev().map(String::as_str).collect();
        assert_eq!(quiet_after(rev[0], &rev[1..]), None, "out of reach");
        // refused, interrupted: no end of its own
        let all = [Turn::by_text("go").says("working").records, turn.records.clone()].concat();
        let rev: Vec<&str> = all.iter().rev().map(String::as_str).collect();
        assert_eq!(quiet_after(rev[0], &rev[1..]), None);
    }
}
