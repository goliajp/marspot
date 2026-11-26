use std::io::Write;

use super::*;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-turns-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn append(path: &Path, lines: &[String]) {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn ts(s: u32) -> String {
    format!("2026-11-26T18:{:02}:{:02}.000Z", s / 60, s % 60)
}

fn at(s: u32) -> i64 {
    marspot::cc_usage::parse_iso_utc(ts(s)).unwrap()
}

fn shell_started(s: u32, id: &str) -> String {
    format!(r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t{id}","content":"Command running in background with ID: {id}"}}]}},"toolUseResult":{{"backgroundTaskId":"{id}"}}}}"#, ts(s))
}

fn agent_started(s: u32, id: &str) -> String {
    format!(r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"a{id}","content":"started"}}]}},"toolUseResult":{{"isAsync":true,"agentId":"{id}"}}}}"#, ts(s))
}

fn notice(s: u32, id: &str, status: &str) -> String {
    format!(r#"{{"type":"queue-operation","operation":"enqueue","timestamp":"{}","content":"<task-notification><task-id>{id}</task-id><status>{status}</status></task-notification>"}}"#, ts(s))
}

fn prompt(s: u32, text: &str) -> String {
    format!(r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":"{text}"}}}}"#, ts(s))
}

fn reply(s: u32, ctx: u64) -> String {
    format!(r#"{{"type":"assistant","timestamp":"{}","message":{{"role":"assistant","stop_reason":"end_turn","usage":{{"input_tokens":{ctx},"output_tokens":0}},"content":[]}}}}"#, ts(s))
}

fn compact(s: u32, pre: u64) -> String {
    format!(r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{}","compactMetadata":{{"trigger":"auto","preTokens":{pre}}}}}"#, ts(s))
}

/// Pane 481 on 2026-11-26: three shells a timed-out command moved to
/// the background, never given an end notice, and an agent that ended.
#[test]
fn allocation_turns_count_a_shell_without_a_notice_only_while_a_process_runs_under_claude() {
    let d = dir("shells");
    let t = d.join("s.jsonl");
    append(&t, &[shell_started(0, "b1"), shell_started(60, "b2"), agent_started(120, "a1"), notice(180, "a1", "completed")]);
    let mut turns = Turns::default();
    assert!(turns.read(&t, 10).is_empty());
    let now = at(600);
    assert_eq!(turns.open(&t, true, now), Open { tasks: 2, oldest_s: Some(600), prompt_at: None });
    assert_eq!(turns.open(&t, false, now).tasks, 0, "no process under claude: the shells are over");
    append(&t, &[agent_started(700, "a2")]);
    turns.read(&t, 10);
    assert_eq!(turns.open(&t, false, at(760)), Open { tasks: 1, oldest_s: Some(60), prompt_at: None }, "an agent has no process to look for");
    append(&t, &[notice(800, "a2", "running")]);
    turns.read(&t, 10);
    assert_eq!(turns.open(&t, false, at(800)).tasks, 1, "a notice with no end status ends nothing");
    turns.read(&t, 11);
    assert_eq!(turns.open(&t, true, at(800)).tasks, 0, "a new claude: the old one's tasks died with it");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_turns_say_a_compaction_with_the_context_before_and_after_once() {
    let d = dir("compact");
    let t = d.join("s.jsonl");
    append(&t, &[reply(0, 900_000), compact(10, 912_688)]);
    let mut turns = Turns::default();
    assert!(turns.read(&t, 1).is_empty(), "what was there when the shell started is history");
    // its first reply came after: no shell saw the compaction whole before
    append(&t, &[reply(30, 400)]);
    assert_eq!(turns.read(&t, 1), [Compacted { at: at(10), before: 912_688, after: 400, auto: true }]);
    append(&t, &[compact(40, 500_000), prompt(50, "go on")]);
    assert!(turns.read(&t, 1).is_empty(), "no reply after it yet");
    append(&t, &[reply(60, 61_000), reply(70, 62_000)]);
    assert_eq!(turns.read(&t, 1), [Compacted { at: at(40), before: 500_000, after: 61_000, auto: true }]);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_turns_know_when_a_person_last_wrote_not_a_notice() {
    let d = dir("prompt");
    let t = d.join("s.jsonl");
    append(&t, &[prompt(5, "fix the test")]);
    let mut turns = Turns::default();
    turns.read(&t, 1);
    append(&t, &[prompt(30, "<task-notification><task-id>x</task-id></task-notification>"), prompt(40, "<local-command-stdout>ok</local-command-stdout>")]);
    turns.read(&t, 1);
    assert_eq!(turns.open(&t, false, at(50)).prompt_at, Some(at(5)));
    // a first read from inside a line starts at the next whole one
    let big = d.join("big.jsonl");
    let pad = format!(r#"{{"type":"progress","pad":"{}"}}"#, "x".repeat(1024));
    append(&big, &vec![pad; (HISTORY / 1024) as usize + 64]);
    append(&big, &[agent_started(1, "a9")]);
    let mut turns = Turns::default();
    turns.read(&big, 1);
    assert_eq!(turns.open(&big, false, at(1)).tasks, 1);
    turns.retain(|p| p != big.as_path());
    assert_eq!(turns.open(&big, false, at(1)), Open::default());
    fs::remove_dir_all(&d).unwrap();
}

/// A line longer than one read (a pasted image) is stepped over, and
/// what follows it is still read.
#[test]
fn allocation_turns_step_over_a_line_longer_than_the_read() {
    let d = dir("longline");
    let t = d.join("s.jsonl");
    fs::write(&t, "").unwrap();
    let mut turns = Turns::default();
    assert!(turns.read(&t, 10).is_empty());
    let image = format!(r#"{{"type":"user","timestamp":"{}","x":"{}"}}"#, ts(20), "A".repeat(READ_CAP as usize + (1 << 20)));
    append(&t, &[prompt(10, "look"), image]);
    assert!(turns.read(&t, 10).is_empty(), "the prompt, and the image line up to the read's end");
    assert_eq!(turns.open(&t, false, at(30)).prompt_at, Some(at(10)));
    assert!(turns.read(&t, 10).is_empty(), "inside the image line");
    append(&t, &[compact(40, 250_000), reply(50, 30_000), prompt(60, "next")]);
    let got = turns.read(&t, 10);
    assert_eq!(got, [Compacted { at: at(40), before: 250_000, after: 30_000, auto: true }]);
    assert_eq!(turns.open(&t, false, at(70)).prompt_at, Some(at(60)));
    fs::remove_dir_all(&d).unwrap();
}
