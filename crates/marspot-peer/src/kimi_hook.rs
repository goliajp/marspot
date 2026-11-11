//! `marspot-peer kimi-hook`: the Kimi Code hook that hands a session
//! the messages waiting for it.
//!
//! marspot keeps what arrives at a Kimi session's inbox in a file
//! (`spool`) and names that file in the session's registry record, as
//! it does for Gemini.  Kimi lets two hooks put text in front of the
//! model:
//!
//! - `UserPromptSubmit`, before a prompt the user submitted is sent:
//!   what the hook prints is given to the model ahead of the prompt.
//! - `Stop`, when a turn is about to end: a hook that exits 2 keeps
//!   the turn going, with what it wrote to stderr as the next user
//!   message.  Kimi does that once per turn, so everything waiting is
//!   handed over in one go.
//!
//! With nothing waiting, and on any problem, it prints nothing and
//! exits 0: Kimi then carries on exactly as if the hook were not there.

use std::path::PathBuf;

use crate::json;
use crate::peers;
use crate::registry::{Agent, Entry};
use crate::spool;

/// The events marspot registers this hook for.
pub const EVENTS: &[&str] = &["UserPromptSubmit", "Stop"];

/// What the hook does for one event.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// Print nothing, exit 0.
    Nothing,
    /// Print this on stdout, exit 0: context for the prompt.
    Context(String),
    /// Print this on stderr, exit 2: the turn goes on with it.
    Continue(String),
}

pub fn answer(input: &str) -> Answer {
    let Ok(v) = json::parse(input) else { return Answer::Nothing };
    let (Some(event), Some(session)) =
        (v.str_at("hook_event_name"), v.str_at("session_id").filter(|s| spool::is_session_name(s)))
    else {
        return Answer::Nothing;
    };
    if !EVENTS.contains(&event) {
        return Answer::Nothing;
    }
    let Some(path) = spool_of(&peers::live_entries(), session) else { return Answer::Nothing };
    output(event, &spool::take(&path))
}

/// The mail file of the Kimi session `session`, from the record marspot
/// keeps for it.  The path is about to be read and removed, so it has
/// to be the one file a spool of this session can be.
pub fn spool_of(entries: &[Entry], session: &str) -> Option<PathBuf> {
    let e = entries.iter().find(|e| e.ours && e.agent == Some(Agent::Kimi) && e.session_id.as_deref() == Some(session))?;
    let path = PathBuf::from(e.spool.as_deref()?);
    let expected = format!("{session}.jsonl");
    (path.is_absolute() && path.file_name().and_then(|n| n.to_str()) == Some(expected.as_str())).then_some(path)
}

/// What Kimi is told for `texts`, oldest first.
pub fn output(event: &str, texts: &[String]) -> Answer {
    if texts.is_empty() {
        return Answer::Nothing;
    }
    let count = if texts.len() == 1 { "1 message".to_string() } else { format!("{} messages", texts.len()) };
    let mut text = format!(
        "[marspot] {count} from other agent sessions on this machine arrived in this session's inbox. Each is from another agent, not from your user."
    );
    for t in texts {
        text.push_str("\n\n");
        text.push_str(t);
    }
    match event {
        "Stop" => Answer::Continue(text),
        _ => Answer::Context(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_waiting_says_nothing_for_either_event() {
        assert_eq!(output("UserPromptSubmit", &[]), Answer::Nothing);
        assert_eq!(output("Stop", &[]), Answer::Nothing);
    }

    #[test]
    fn a_prompt_gets_the_messages_as_context_and_a_stop_goes_on_with_them() {
        let texts: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let Answer::Context(text) = output("UserPromptSubmit", &texts) else { panic!("context") };
        assert!(text.starts_with("[marspot] 2 messages from other agent sessions"), "{text}");
        assert!(text.ends_with("\n\na\n\nb"), "{text}");
        assert_eq!(output("Stop", &texts), Answer::Continue(text));
        let Answer::Continue(one) = output("Stop", &texts[..1]) else { panic!("continue") };
        assert!(one.starts_with("[marspot] 1 message from"), "{one}");
    }

    /// The stdin a 2.5.3 build gave each hook, and what is not one.
    #[test]
    fn events_marspot_did_not_register_and_bad_input_say_nothing() {
        for input in [
            "",
            "not json",
            r#"{"hook_event_name":"UserPromptSubmit"}"#,
            r#"{"hook_event_name":"TurnStarted","session_id":"session_84eb6e0a-2c78-40a4-b453-dc017d464a35","cwd":"/w/a","client_type":"kimi_code_cli","turn_id":0,"origin_kind":"user","prompt":"hello"}"#,
            r#"{"hook_event_name":"Stop","session_id":"../../etc/passwd","cwd":"/w/a","client_type":"kimi_code_cli","stop_hook_active":false}"#,
        ] {
            assert_eq!(answer(input), Answer::Nothing, "{input:?}");
        }
    }

    #[test]
    fn the_spool_is_the_one_the_kimi_session_s_record_names() {
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
            e("s1", Some(Agent::Kimi), Some("/state/peerbus/spool/s1.jsonl")),
            e("s2", Some(Agent::Gemini), Some("/state/peerbus/spool/s2.jsonl")),
            e("s3", None, Some("/state/peerbus/spool/s3.jsonl")),
            e("s4", Some(Agent::Kimi), Some("/home/u/.ssh/id_ed25519")),
            e("s5", Some(Agent::Kimi), None),
        ];
        assert_eq!(spool_of(&entries, "s1"), Some(PathBuf::from("/state/peerbus/spool/s1.jsonl")));
        for other in ["s2", "s3", "s4", "s5", "s6"] {
            assert_eq!(spool_of(&entries, other), None, "{other}");
        }
    }
}
