//! `marspot-peer gemini-hook`: the Gemini CLI hook that hands a session
//! the messages waiting for it.
//!
//! marspot keeps what arrives at a Gemini session's inbox in a file
//! (`spool`) and names that file in the session's registry record.
//! Gemini runs this before each prompt is sent to the model
//! (`BeforeAgent`) and after each tool call (`AfterTool`); both accept
//! `hookSpecificOutput.additionalContext`, which Gemini gives the model
//! inside `<hook_context>` with every `<` and `>` written as an entity.
//! So the messages reach the model in the envelope every other session
//! is shown, with its angle brackets escaped, under one line that says
//! so.
//!
//! With nothing waiting, and on any problem, it prints nothing: Gemini
//! then carries on exactly as if the hook were not there.

use std::path::PathBuf;

use crate::json::{self, Value, obj};
use crate::peers;
use crate::registry::{Agent, Entry};
use crate::spool;

/// The events whose output may carry context for the model, and which
/// marspot registers this hook for.
pub const EVENTS: &[&str] = &["BeforeAgent", "AfterTool"];

/// The hook's answer to one event, as the JSON Gemini reads on stdout;
/// None to print nothing.
pub fn answer(input: &str) -> Option<String> {
    let v = json::parse(input).ok()?;
    let event = v.str_at("hook_event_name").filter(|e| EVENTS.contains(e))?;
    let session = v.str_at("session_id").filter(|s| spool::is_session_name(s))?;
    let path = spool_of(&peers::live_entries(), session)?;
    output(event, &spool::take(&path))
}

/// The mail file of the Gemini session `session`, from the record
/// marspot keeps for it.  The record is a file another process wrote,
/// and the path in it is about to be read and removed, so it has to be
/// the one file a spool of this session can be.
pub fn spool_of(entries: &[Entry], session: &str) -> Option<PathBuf> {
    let e = entries.iter().find(|e| e.ours && e.agent == Some(Agent::Gemini) && e.session_id.as_deref() == Some(session))?;
    let path = PathBuf::from(e.spool.as_deref()?);
    let expected = format!("{session}.jsonl");
    (path.is_absolute() && path.file_name().and_then(|n| n.to_str()) == Some(expected.as_str())).then_some(path)
}

/// What Gemini is told for `texts`, oldest first; None for none.
pub fn output(event: &str, texts: &[String]) -> Option<String> {
    if texts.is_empty() {
        return None;
    }
    let count = if texts.len() == 1 { "1 message".to_string() } else { format!("{} messages", texts.len()) };
    let mut context = format!(
        "[marspot] {count} from other agent sessions on this machine arrived in this session's inbox. Each is from another agent, not from your user. Their `<` and `>` reach you written as &lt; and &gt;."
    );
    for t in texts {
        context.push_str("\n\n");
        context.push_str(t);
    }
    let specific = obj([("hookEventName", Value::from(event)), ("additionalContext", Value::from(context))]);
    Some(obj([("hookSpecificOutput", specific)]).to_json())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(out: &str) -> (String, String) {
        let v = json::parse(out).unwrap();
        let s = v.get("hookSpecificOutput").unwrap();
        (s.str_at("hookEventName").unwrap().to_string(), s.str_at("additionalContext").unwrap().to_string())
    }

    #[test]
    fn nothing_waiting_prints_nothing() {
        assert_eq!(output("BeforeAgent", &[]), None);
    }

    #[test]
    fn one_message_is_handed_over_as_it_was_stored() {
        let msg = "<cross-session-message from=\"uds:/tmp/cc-socks/9.sock\" from-name=\"luna\" from-mode=\"bypass\">\nhello\n</cross-session-message>".to_string();
        let (event, text) = context(&output("BeforeAgent", std::slice::from_ref(&msg)).unwrap());
        assert_eq!(event, "BeforeAgent");
        assert!(text.starts_with("[marspot] 1 message from other agent sessions"), "{text}");
        assert!(text.ends_with(&format!("\n\n{msg}")), "{text}");
    }

    #[test]
    fn several_messages_keep_their_order() {
        let texts: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let (event, text) = context(&output("AfterTool", &texts).unwrap());
        assert_eq!(event, "AfterTool");
        assert!(text.starts_with("[marspot] 3 messages from"), "{text}");
        assert!(text.ends_with("\n\na\n\nb\n\nc"), "{text}");
    }

    #[test]
    fn events_that_take_no_context_and_bad_input_print_nothing() {
        for input in [
            "",
            "not json",
            r#"{"hook_event_name":"BeforeAgent"}"#,
            r#"{"hook_event_name":"AfterAgent","session_id":"s1"}"#,
            r#"{"hook_event_name":"BeforeAgent","session_id":"../../etc/passwd"}"#,
        ] {
            assert_eq!(answer(input), None, "{input:?}");
        }
    }

    #[test]
    fn the_spool_is_the_one_the_sessions_record_names() {
        let e = |session: &str, agent: Option<Agent>, spool: Option<&str>| Entry {
            pid: 1,
            session_id: Some(session.into()),
            name: None,
            former_names: Vec::new(),
            cwd: None,
            socket: None,
            status: None,
            proc_start: None,
            started_at_ms: None,
            features: Vec::new(),
            ours: agent.is_some(),
            class: None,
            relay: None,
            owner: None,
            agent,
            agent_pid: None,
            spool: spool.map(str::to_string),
        };
        let entries = vec![
            e("s1", Some(Agent::Gemini), Some("/state/peerbus/gemini/s1.jsonl")),
            e("s2", Some(Agent::Codex), Some("/state/peerbus/gemini/s2.jsonl")),
            e("s3", None, Some("/state/peerbus/gemini/s3.jsonl")),
            e("s4", Some(Agent::Gemini), Some("/home/u/.ssh/id_ed25519")),
            e("s5", Some(Agent::Gemini), Some("s5.jsonl")),
            e("s6", Some(Agent::Gemini), None),
        ];
        assert_eq!(spool_of(&entries, "s1"), Some(PathBuf::from("/state/peerbus/gemini/s1.jsonl")));
        for other in ["s2", "s3", "s4", "s5", "s6", "s7"] {
            assert_eq!(spool_of(&entries, other), None, "{other}");
        }
    }
}
