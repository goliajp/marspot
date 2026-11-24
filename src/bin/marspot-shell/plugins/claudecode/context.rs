//! How large a session's context is, as its transcript's tail says.

use std::path::Path;

use marspot_peer::json::{self, Value};

use super::transcript::record_type;
use super::with_helpers::turns;
use super::worker::WorkerCtx;
use crate::plugins::helpers::used::{self, Used};

/// What one `usage` object counts: what was sent and what was said.
fn counted(usage: &Value) -> Option<(u64, u64)> {
    let n = |k: &str| usage.get(k).and_then(Value::i64).and_then(|n| u64::try_from(n).ok());
    let sent = [n("input_tokens")?, n("cache_creation_input_tokens").unwrap_or(0), n("cache_read_input_tokens").unwrap_or(0)];
    let sent = sent.into_iter().fold(0, u64::saturating_add);
    Some((sent, n("output_tokens").unwrap_or(0)))
}

/// The context of the response a record is of.  One whose usage gives
/// nothing sent may say it by iteration.
fn tokens_of(line: &str) -> Option<u64> {
    let v = json::parse(line).ok()?;
    let usage = v.at(&["message", "usage"])?;
    let (mut sent, mut said) = counted(usage)?;
    if sent == 0 {
        let own = |i: &&Value| !i.str_at("type").is_some_and(|t| t.contains("advisor") || t.contains("compaction"));
        (sent, said) = usage.get("iterations")?.arr().iter().rev().find(own).and_then(counted)?;
    }
    Some(sent.saturating_add(said)).filter(|n| *n > 0)
}

/// What `tail` says; `whole`: it is all of the transcript.
pub(super) fn used_in(tail: &str, whole: bool) -> Used {
    let mut used = Used { tokens: None, had_tool: whole.then_some(false) };
    let own = tail.lines().rev().filter(|l| record_type(l) == Some("assistant") && !l.contains(r#""isSidechain":true"#));
    for line in own {
        used.tokens = used.tokens.or_else(|| tokens_of(line));
        if line.contains(r#""type":"tool_use""#) {
            used.had_tool = Some(true);
        }
        if used.tokens.is_some() && used.had_tool == Some(true) {
            break;
        }
    }
    used
}

impl WorkerCtx {
    /// Say how large the context of the helper in pane `sid` is, from
    /// the tail the scan read.
    pub(super) fn note_context(&mut self, sid: u64, bound_by: &str, transcript: Option<&Path>) {
        if let Some(path) = transcript
            && turns::named_by_claude(bound_by)
            && used::wanted(sid)
            && let Some(conv) = path.file_stem()
        {
            used::read(sid, &conv.to_string_lossy(), self.caches.tails.used(path).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAID: &str = r#"{"isSidechain":false,"message":{"id":"m1","content":[{"type":"text","text":"x"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":281,"cache_read_input_tokens":11829,"output_tokens":96,"iterations":[{"input_tokens":2,"type":"message"}]}},"type":"assistant"}"#;
    const TOOL: &str = r#"{"isSidechain":false,"message":{"id":"m0","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}],"usage":{"input_tokens":2,"cache_creation_input_tokens":11829,"cache_read_input_tokens":0,"output_tokens":179}},"type":"assistant"}"#;
    const USER: &str = r#"{"type":"user","message":{"content":"\"usage\":{\"input_tokens\":9}"}}"#;
    const REFUSED: &str = r#"{"message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"type":"assistant"}"#;

    fn of(lines: &[&str], whole: bool) -> (Option<u64>, Option<bool>) {
        let u = used_in(&lines.join("\n"), whole);
        (u.tokens, u.had_tool)
    }

    #[test]
    fn claudecode_context_is_the_last_responses_and_not_a_sum() {
        assert_eq!(of(&[TOOL, USER, SAID, SAID, USER], true), (Some(12_208), Some(true)), "one response is several records");
        assert_eq!(of(&[USER, TOOL], true), (Some(12_010), Some(true)));
        assert_eq!(of(&[USER, SAID], true), (Some(12_208), Some(false)));
        assert_eq!(of(&[USER, SAID], false), (Some(12_208), None), "a tool may have been called before");
        assert_eq!(of(&[SAID, REFUSED], true).0, Some(12_208), "an error is no response");
        let aside = TOOL.replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
        assert_eq!(of(&[SAID, &aside], true), (Some(12_208), Some(false)));
        assert_eq!(of(&[USER], true).0, None);
        let most = SAID.replace(r#""input_tokens":2,"c"#, r#""input_tokens":9223372036854775807,"c"#).replace("281", "9223372036854775807").replace(":96", ":9223372036854775807");
        assert_eq!(of(&[&most], true).0, Some(u64::MAX));
        // the window starts inside a record
        assert_eq!(of(&[&SAID[40..], USER], false), (None, None));
    }

    #[test]
    fn claudecode_context_comes_from_the_iterations_when_nothing_was_sent() {
        let by_iteration = r#"{"message":{"usage":{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":7,"iterations":[{"type":"message","input_tokens":1,"cache_read_input_tokens":500,"output_tokens":20},{"type":"message","input_tokens":3,"cache_read_input_tokens":900,"output_tokens":40},{"type":"compaction","input_tokens":9000,"output_tokens":1},{"type":"advisor_message","input_tokens":8000,"output_tokens":1}]}},"type":"assistant"}"#;
        assert_eq!(of(&[SAID, by_iteration], true).0, Some(943));
    }

    #[test]
    fn claudecode_context_is_worked_out_once_per_file_version() {
        let p = std::env::temp_dir().join(format!("marspot-context-{}.jsonl", std::process::id()));
        std::fs::write(&p, format!("{USER}\n{SAID}\n")).unwrap();
        let mut tails = super::super::transcript::TailCache::default();
        assert_eq!(tails.used(&p), None, "no tail was read: none is read for this");
        for _ in 0..3 {
            tails.tail(&p).unwrap();
            assert_eq!(tails.used(&p), Some(Used { tokens: Some(12_208), had_tool: Some(false) }));
        }
        assert_eq!(tails.reads, 1);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, format!("{TOOL}\n").as_bytes()).unwrap();
        tails.tail(&p).unwrap();
        assert_eq!(tails.used(&p), Some(Used { tokens: Some(12_010), had_tool: Some(true) }));
        assert_eq!(tails.reads, 2);
        std::fs::remove_file(&p).unwrap();
    }
}
