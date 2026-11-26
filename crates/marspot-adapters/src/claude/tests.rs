use marspot_agent::types::{Compacted, Reply, SubLast, TaskChange, TurnState};

use super::*;

#[derive(Default)]
struct Seen {
    replies: Vec<Reply>,
    changes: Vec<String>,
}

impl FoldSink for Seen {
    fn used(&mut self, r: &Reply) {
        self.replies.push(*r);
    }
    fn compacted(&mut self, _: Compacted) {}
    fn task(&mut self, c: TaskChange<'_>) {
        self.changes.push(format!("{c:?}"));
    }
}

const CALL: &str = r#"{"type":"assistant","timestamp":"2026-11-24T14:00:00Z","message":{"id":"m1","model":"claude-opus-5-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"sleep 9","run_in_background":true}}],"usage":{"input_tokens":3,"cache_read_input_tokens":1000,"output_tokens":20}}}"#;
const RESULT: &str = r#"{"type":"user","timestamp":"2026-11-24T14:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"Command running in background with ID: b42. Output is being written"}]}}"#;
const NOTE: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-24T14:00:09Z","content":"<task-notification><task-id>b42</task-id><status>completed</status></task-notification>"}"#;
const END: &str = r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-11-24T14:00:10Z"}"#;

fn record_of(lines: &[&str]) -> Vec<u8> {
    lines.iter().flat_map(|l| format!("{l}\n").into_bytes()).collect()
}

#[test]
fn claude_fold_follows_a_background_task_from_its_call_to_its_end() {
    let bytes = record_of(&[CALL, CALL, RESULT, NOTE, END]);
    let (mut st, mut seen) = (RecordFold::from_start(), Seen::default());
    fold(&bytes, &mut st, &mut seen);
    assert_eq!(st.at, bytes.len() as u64);
    assert_eq!(st.turn, TurnState::Ended { at: 1_795_528_810 });
    assert_eq!(st.ctx_tokens, Some(1023));
    assert_eq!(st.model.map(|m| m.as_str().to_string()), Some("claude-opus-5-5".into()));
    assert_eq!(st.called_tool, Some(true));
    assert_eq!(seen.replies.len(), 2);
    assert!(!seen.replies[0].repeat && seen.replies[1].repeat, "the second record is the same reply");
    assert!(seen.replies[1].grew.is_empty(), "and adds nothing");
    assert_eq!(st.spent.output, 20);
    let b42 = st.tasks.tasks().iter().find(|t| t.id.is("b42")).copied().unwrap();
    assert_eq!(b42.started.map(|s| (s.known_call, s.background)), Some((true, true)));
    assert_eq!((b42.ended_at, b42.finished_at), (Some(1_795_528_809), Some(1_795_528_809)));
    assert_eq!(st.tasks.open_calls(), 0);
    assert_eq!(st.tasks.last, SubLast::Busy);
    assert!(seen.changes[0].contains(r#"command: Some("sleep 9")"#), "{:?}", seen.changes);
}

#[test]
fn claude_fold_takes_a_line_once_it_is_whole() {
    let bytes = record_of(&[CALL, RESULT]);
    let (mut st, mut seen) = (RecordFold::from_start(), Seen::default());
    let cut = CALL.len() / 2;
    fold(&bytes[..cut], &mut st, &mut seen);
    assert_eq!((st.at, seen.replies.len()), (0, 0), "half a line waits");
    fold(&bytes[st.at as usize..], &mut st, &mut seen);
    assert_eq!((st.at, seen.replies.len()), (bytes.len() as u64, 1));
}

#[test]
fn claude_fold_from_the_middle_skips_the_line_it_landed_in() {
    let bytes = record_of(&[CALL, RESULT]);
    let from = 10u64;
    let (mut st, mut seen) = (RecordFold::from(from), Seen::default());
    fold(&bytes[from as usize..from as usize + 5], &mut st, &mut seen);
    assert!(st.mid_record, "no newline yet");
    fold(&bytes[st.at as usize..], &mut st, &mut seen);
    assert_eq!(seen.replies.len(), 0, "the call's record was cut");
    assert_eq!(st.called_tool, None, "not read from the top");
    assert_eq!(st.tasks.tasks()[0].started.map(|s| s.known_call), Some(false), "a result whose call was not read");
}

#[test]
fn claude_fold_from_the_end_of_a_file_counts_the_first_appended_record() {
    let before = record_of(&[RESULT, END]);
    let bytes = [before.clone(), record_of(&[CALL, END])].concat();
    let (mut st, mut seen) = (RecordFold::from_boundary(before.len() as u64), Seen::default());
    fold(&bytes[before.len()..], &mut st, &mut seen);
    assert_eq!(seen.replies.len(), 1, "the first record after the start is whole");
    assert_eq!((st.spent.output, st.ctx_tokens), (20, Some(1023)));
    assert_eq!(st.at, bytes.len() as u64);
}
