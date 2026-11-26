//! How large a session's context is, as its transcript's tail says.

use std::path::Path;

use marspot_peer::json::{self, Value};

use super::model::short_model;
use super::transcript::record_type;
use super::transcript::tail_reach::{MOST, read_back};
use super::with_helpers::turns;
use super::worker::WorkerCtx;
use crate::plugins::helpers::used::{self, Model, Used};

/// What one `usage` object counts: what was sent and what was said.
fn counted(usage: &Value) -> Option<(u64, u64)> {
    let n = |k: &str| usage.get(k).and_then(Value::i64).and_then(|n| u64::try_from(n).ok());
    let sent = [n("input_tokens")?, n("cache_creation_input_tokens").unwrap_or(0), n("cache_read_input_tokens").unwrap_or(0)];
    let sent = sent.into_iter().fold(0, u64::saturating_add);
    Some((sent, n("output_tokens").unwrap_or(0)))
}

/// The family of a claude model, as its badge starts: `opus` of
/// `claude-opus-4-8`.
fn family(raw: &str) -> Option<Model> {
    Model::new(short_model(raw).split('-').next().unwrap_or_default())
}

/// The context of the response a record is of, and the model that gave
/// it.  One whose usage gives nothing sent may say it by iteration.
fn response_of(line: &str) -> (Option<u64>, Option<Model>) {
    let Ok(v) = json::parse(line) else { return (None, None) };
    (tokens_of(&v), v.at(&["message", "model"]).and_then(Value::str).and_then(family))
}

fn tokens_of(v: &Value) -> Option<u64> {
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
    let mut used = Used { had_tool: whole.then_some(false), ..Used::default() };
    let own = tail.lines().rev().filter(|l| record_type(l) == Some("assistant") && !l.contains(r#""isSidechain":true"#));
    for line in own {
        if used.tokens.is_none() {
            let (tokens, model) = response_of(line);
            used.tokens = tokens;
            used.model = used.model.or(model);
        }
        if line.contains(r#""type":"tool_use""#) {
            used.had_tool = Some(true);
        }
        if used.tokens.is_some() && used.had_tool == Some(true) {
            break;
        }
    }
    used
}

/// What the `tail` of the transcript at `path`, `len` bytes long, says;
/// the last response may be one record longer than the tail.
pub(super) fn used_of_tail(path: &Path, tail: &str, len: u64, window: u64) -> Used {
    let used = used_in(tail, len <= window);
    match used.tokens {
        None if len > window => {
            let far = response_before(path, len, window);
            Used { tokens: far.tokens, model: used.model.or(far.model), ..used }
        }
        _ => used,
    }
}

/// The context of the last response in the transcript at `path`, `len`
/// bytes long, and its model, read in windows twice as long each time
/// from twice `near` up to [`MOST`]: a record longer than the tail cuts
/// it in two.  Nothing when the tail already reached that far.
fn response_before(path: &Path, len: u64, near: u64) -> Used {
    if near >= MOST {
        return Used::default();
    }
    let mut used = Used::default();
    let read = read_back(path, len, near.saturating_mul(2).min(MOST), 2, |buf, start| {
        let text = String::from_utf8_lossy(buf);
        // a window that starts inside a record leaves that record out
        let whole = if start == 0 { &text[..] } else { text.split_once('\n').map_or("", |(_, rest)| rest) };
        used = used_in(whole, start == 0);
        used.tokens.is_some()
    });
    read.map_or_else(Used::default, |_| used)
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
    fn claudecode_context_names_the_model_of_the_last_response_by_its_family() {
        let by = |id: &str| SAID.replace(r#""message":{"id":"m1","#, &format!(r#""message":{{"id":"m1","model":"{id}","#));
        let model = |lines: &[&str]| used_in(&lines.join("\n"), true).model.map(|m| m.as_str().to_string());
        assert_eq!(model(&[&by("claude-sonnet-5"), &by("claude-opus-4-8")]), Some("opus".into()), "the last response's");
        assert_eq!(model(&[&by("claude-haiku-4-5-20251001"), REFUSED]), Some("haiku".into()), "an error names no model");
        assert_eq!(model(&[&by("claude-fable-5"), USER]), Some("fable".into()));
        assert_eq!(model(&[SAID]), None, "a record that names none");
        let p = std::env::temp_dir().join(format!("marspot-context-model-{}.jsonl", std::process::id()));
        let long = by("claude-opus-4-8").replace(r#""text":"x""#, &format!(r#""text":"{}""#, "x".repeat(100 * 1024)));
        std::fs::write(&p, format!("{}\n{long}\n", by("claude-sonnet-5"))).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        let tail = "x".repeat(10);
        assert_eq!(used_of_tail(&p, &tail, len, 1024).model.map(|m| m.as_str().to_string()), Some("opus".into()), "past a long last record");
        std::fs::remove_file(&p).unwrap();
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
            assert_eq!(tails.used(&p), Some(Used { tokens: Some(12_208), had_tool: Some(false), model: None }));
        }
        assert_eq!(tails.reads, 1);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, format!("{TOOL}\n").as_bytes()).unwrap();
        tails.tail(&p).unwrap();
        assert_eq!(tails.used(&p), Some(Used { tokens: Some(12_010), had_tool: Some(true), model: None }));
        assert_eq!(tails.reads, 2);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn claudecode_context_is_read_from_a_record_longer_than_the_tail() {
        let p = std::env::temp_dir().join(format!("marspot-context-long-{}.jsonl", std::process::id()));
        let long = TOOL.replace(r#""input":{}"#, &format!(r#""input":{{"content":"{}"}}"#, "x".repeat(100 * 1024)));
        std::fs::write(&p, format!("{USER}\n{SAID}\n{USER}\n{long}\n")).unwrap();
        let mut tails = super::super::transcript::TailCache::default();
        tails.tail(&p).unwrap();
        assert_eq!(tails.used(&p).unwrap().tokens, Some(12_010), "the long record is the last response");
        let len = std::fs::metadata(&p).unwrap().len();
        assert_eq!(response_before(&p, len, 1024).tokens, Some(12_010));
        let user_of = |n: u64| format!(r#"{{"type":"user","message":{{"content":"{}"}}}}"#, "y".repeat(n as usize));
        std::fs::write(&p, format!("{SAID}\n{}\n", user_of(200 * 1024))).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        assert_eq!(response_before(&p, len, 1024).tokens, Some(12_208), "past a long record of another kind");
        std::fs::write(&p, format!("{SAID}\n{}\n", user_of(MOST))).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        assert_eq!(response_before(&p, len, 1024).tokens, None, "no further back than the bound");
        std::fs::remove_file(&p).unwrap();
    }
}

#[cfg(test)]
#[path = "fold_check_tests.rs"]
mod fold_check_tests;
