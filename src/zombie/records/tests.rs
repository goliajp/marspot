use super::*;

// lines in the shape claude 2.1.33x writes, cut down
const CALL: &str = r#"{"parentUuid":"p","message":{"model":"m","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"bash w.sh --stopped 3600","run_in_background":true,"timeout":600000}}],"stop_reason":"tool_use"},"type":"assistant","timestamp":"2026-11-25T05:57:06.953Z"}"#;
const STARTED: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"Command running in background with ID: b6lpc8u81. Output is being written to: /x/tasks/b6lpc8u81.output."}]},"timestamp":"2026-11-25T05:57:07.016Z"}"#;
const MONITOR: &str = r#"{"type":"user","message":{"content":[{"tool_use_id":"toolu_2","type":"tool_result","content":[{"type":"text","text":"Monitor started (task bkssmnnza, expires in 30m)"}]}]},"timestamp":"2026-11-23T00:57:41.678Z"}"#;
const QUEUED: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-23T01:09:44.682Z","content":"<task-notification>\n<task-id>bkssmnnza</task-id>\n<status>completed</status>\n<summary>Monitor stream ended</summary>\n</task-notification>"}"#;
const WAITING: &str = r#"{"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>aa03fffa738e7db41</task-id>\n<status>completed</status>\n<summary>Agent \"Step 3b\" is waiting on its own background work</summary>\n</task-notification>"},"timestamp":"2026-11-23T00:57:43.493Z"}"#;
const EVENT: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-23T01:19:53.954Z","content":"<task-notification><task-id>b62fgn46t</task-id><summary>Monitor event: \"x\"</summary></task-notification>"}"#;
const TURN: &str = r#"{"parentUuid":"x","isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":1,"timestamp":"2026-11-23T00:57:47.673Z"}"#;
const REPLY: &str = r#"{"message":{"type":"message","role":"assistant","content":[{"type":"text","text":"Waiting for the build."}],"stop_reason":"end_turn"},"type":"assistant","timestamp":"2026-11-23T00:57:43.406Z"}"#;

fn t(s: &str) -> u64 {
    crate::cc_usage::parse_iso_utc(s.to_string()).unwrap() as u64
}

#[test]
fn zombie_records_read_a_background_call_and_the_task_it_started() {
    let Some(Rec::Assistant { at, uses, end }) = parse(CALL) else { panic!("an assistant record") };
    assert_eq!(at, t("2026-11-25T05:57:06Z"));
    assert!(!end);
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].id, "toolu_1");
    assert!(uses[0].background);
    assert_eq!(uses[0].command.as_deref(), Some("bash w.sh --stopped 3600"));
    assert_eq!(uses[0].timeout_ms, Some(600_000));
    let Some(Rec::User { results, notes, .. }) = parse(STARTED) else { panic!("a user record") };
    assert_eq!(results, vec![("toolu_1".to_string(), Some("b6lpc8u81".to_string()))]);
    assert!(notes.is_empty());
    let Some(Rec::User { results, .. }) = parse(MONITOR) else { panic!("a user record") };
    assert_eq!(results[0].1.as_deref(), Some("bkssmnnza"), "a monitor's result as a list of text blocks");
}

#[test]
fn zombie_records_read_notifications_ends_and_turns() {
    let Some(Rec::Queued { notes, .. }) = parse(QUEUED) else { panic!("queued") };
    assert_eq!(notes, vec![Note { task: "bkssmnnza".into(), ended: true, waiting: false }]);
    let Some(Rec::User { notes, .. }) = parse(WAITING) else { panic!("user") };
    assert_eq!(notes, vec![Note { task: "aa03fffa738e7db41".into(), ended: true, waiting: true }]);
    let Some(Rec::Queued { notes, .. }) = parse(EVENT) else { panic!("queued") };
    assert!(!notes[0].ended, "a monitor's event carries no status and ends nothing");
    assert_eq!(parse(TURN), Some(Rec::TurnEnd { at: t("2026-11-23T00:57:47Z") }));
    let Some(Rec::Assistant { uses, end, .. }) = parse(REPLY) else { panic!("assistant") };
    assert!(end && uses.is_empty());
}

#[test]
fn zombie_records_read_every_task_of_a_notification_that_names_several() {
    let line = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-12T04:33:14.795Z","content":"<task-notification>\n<task-id>adcf449af71a73dd4</task-id>\n<task-id>aa933e36dcf92f20b</task-id>\n<status>stopped</status>\n<summary>2 background agents didn't finish before the previous session ended</summary>\n</task-notification>"}"#;
    let Some(Rec::Queued { notes, .. }) = parse(line) else { panic!("queued") };
    let ids: Vec<&str> = notes.iter().map(|n| n.task.as_str()).collect();
    assert_eq!(ids, ["adcf449af71a73dd4", "aa933e36dcf92f20b"]);
    assert!(notes.iter().all(|n| n.ended && !n.waiting));
}

#[test]
fn zombie_records_skip_everything_else() {
    for line in [
        r#"{"type":"data-flow-records","v":1,"records":[]}"#,
        r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-11-23T00:32:49.669Z","content":"<task-notification><task-id>a</task-id><status>completed</status></task-notification>"}"#,
        r#"{"type":"system","subtype":"away_summary","timestamp":"2026-11-25T04:53:57.864Z"}"#,
        r#"{"type":"assistant","message":{"content":[]}}"#,
        "not json \"user\"",
    ] {
        assert_eq!(parse(line), None, "{line}");
    }
}
