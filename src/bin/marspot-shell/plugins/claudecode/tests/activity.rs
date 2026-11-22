//! Classifying what a transcript says claude is doing, from its last records.

use super::*;

// ── cc activity classification ────────────────────────────────
// The literals below are trimmed copies of real records from a
// live transcript (`~/.claude/projects/…/<uuid>.jsonl`), not
// invented shapes — the whole classification rests on where
// `"type"` appears, so a guessed shape would test nothing.

const REC_TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#;

const REC_TEXT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;

const REC_THINKING: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"}]}}"#;

const REC_TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}"#;

const REC_USER_TEXT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"go on"}]}}"#;

const REC_SYSTEM: &str = r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>"}"#;

/// An assistant turn that ended with a message is the one state
/// where nothing is in flight.
#[test]
fn activity_assistant_message_is_awaiting_user() {
    assert_eq!(
        activity_from_last_record(REC_TEXT, false),
        CcActivity::AwaitingUser
    );
    assert_eq!(
        activity_from_last_record(REC_THINKING, false),
        CcActivity::AwaitingUser
    );
}

/// A tool call with nothing after it: running when claude has a
/// child younger than the record, parked on the approval prompt
/// when it has none.  Both are "in flight" — the split is for the
/// log line, and neither may be read as idle.
#[test]
fn activity_tool_use_splits_on_whether_a_child_is_running() {
    assert_eq!(
        activity_from_last_record(REC_TOOL_USE, true),
        CcActivity::ToolPending { executing: true }
    );
    assert_eq!(
        activity_from_last_record(REC_TOOL_USE, false),
        CcActivity::ToolPending { executing: false }
    );
    assert_ne!(
        activity_from_last_record(REC_TOOL_USE, false),
        CcActivity::AwaitingUser,
        "an unanswered tool call is never idle — this is the case \
         that makes killing claude lose a pending call"
    );
}

/// Both user-record flavours (a real prompt, and the transcript's
/// record of a tool result) leave the assistant owing the next
/// record.
#[test]
fn activity_user_records_mean_the_assistant_owes_a_turn() {
    assert_eq!(
        activity_from_last_record(REC_USER_TEXT, false),
        CcActivity::Working
    );
    assert_eq!(
        activity_from_last_record(REC_TOOL_RESULT, false),
        CcActivity::Working
    );
}

/// Anything not modelled reads as Unknown, which callers treat as
/// "in flight" — never as idle.
#[test]
fn activity_unmodelled_shapes_are_unknown_not_idle() {
    for line in [REC_SYSTEM, "", "not json at all", r#"{"no_type":1}"#] {
        let got = activity_from_last_record(line, false);
        assert_eq!(got, CcActivity::Unknown, "line: {line}");
    }
}

/// The labels land in logs on both sides of the report channel,
/// so pin them — they are what a future reader greps for.
#[test]
fn activity_label_strings_are_stable() {
    assert_eq!(CcActivity::AwaitingUser.label(), "awaiting_user");
    assert_eq!(
        CcActivity::ToolPending { executing: true }.label(),
        "tool_executing"
    );
    assert_eq!(
        CcActivity::ToolPending { executing: false }.label(),
        "tool_awaiting_approval"
    );
    assert_eq!(CcActivity::Working.label(), "working");
    assert_eq!(CcActivity::Unknown.label(), "unknown");
}

/// End-to-end through the file read, including the "last line
/// wins" rule.
#[test]
fn tail_activity_classifies_the_final_record_of_a_file() {
    let path = tmpfile(&format!(
        "{REC_USER_TEXT}\n{REC_TOOL_USE}\n{REC_TOOL_RESULT}\n{REC_TEXT}\n"
    ));
    assert_eq!(tail_activity(&path, false), CcActivity::AwaitingUser);
    let path = tmpfile(&format!("{REC_TEXT}\n{REC_TOOL_USE}\n"));
    assert_eq!(
        tail_activity(&path, false),
        CcActivity::ToolPending { executing: false }
    );
    let missing = PathBuf::from("/nonexistent/marspot-cc-activity.jsonl");
    assert_eq!(tail_activity(&missing, false), CcActivity::Unknown);
}

// ── real transcript lines ─────────────────────────────────────
// Verbatim prefixes of live records (`~/.claude/projects/…`),
// truncated only in the payload.  The records above were written
// with `"type"` first, which is NOT the on-disk field order, and
// the classifier reading only the literal last line was wrong on 7
// of 9 live panes — both facts are only visible against the real
// thing.

const LIVE_TOOL_RESULT: &str = r#"{"parentUuid":"22efebbb-9e50-42b8-a263-0870f70dde24","isSidechain":false,"promptId":"df17b9a2-3099-4e1c-a7ca-2405556f6a85","type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01SRfj2obe7VZK5ykPYFbcGz","type":"tool_result","content":"…"}]}}"#;

const LIVE_TURN_DURATION: &str = r#"{"parentUuid":"d387726e-c410-4213-84b6-c73a3e604ddd","isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":29074,"messageCount":1433,"timestamp":"2026-07-30T18:31:52.149Z","isMeta":false}"#;

/// NOTE the field order: `message` (carrying its own
/// `"type":"message"`) comes BEFORE the record's own `"type"`.
/// That is how claudecode actually writes assistant records, and
/// reading the first `"type"` in the line therefore yields
/// `message` — the bug 0.7.38 shipped.
const LIVE_ASSISTANT_TEXT: &str = r#"{"parentUuid":"4809f00f-b471-4330-a9e3-870374a08f67","isSidechain":false,"message":{"model":"claude-fable-5","id":"msg_011CdTzjoRr7Nb2u3TumMno6","type":"message","role":"assistant","content":[{"type":"text","text":"这条唤醒滞后"}]},"type":"assistant","uuid":"a261e9a7","timestamp":"2026-07-28T06:39:12.718Z"}"#;

const LIVE_ASSISTANT_TOOL_USE: &str = r#"{"parentUuid":"4809f00f","isSidechain":false,"message":{"model":"claude-fable-5","id":"msg_02","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"ls"}}]},"type":"assistant","uuid":"b1"}"#;

const LIVE_LAST_PROMPT: &str = r#"{"type":"last-prompt","lastPrompt":"文档、网站都要更新","leafUuid":"a261e9a7","sessionId":"f59436fa"}"#;

/// The record's own type is the TOP-LEVEL one — not the first
/// `"type"` in the line, which for assistant records belongs to
/// the nested `message` object.
#[test]
fn record_type_reads_the_records_own_type_from_a_live_line() {
    assert_eq!(record_type(LIVE_TOOL_RESULT), Some("user"));
    assert_eq!(record_type(LIVE_TURN_DURATION), Some("system"));
    assert_eq!(record_type(LIVE_LAST_PROMPT), Some("last-prompt"));
    assert_eq!(
        record_type(LIVE_ASSISTANT_TEXT),
        Some("assistant"),
        "nested message.type must not win — this is the 0.7.38 bug"
    );
    assert_eq!(record_type(LIVE_ASSISTANT_TOOL_USE), Some("assistant"));
}

/// Depth is what makes it right, so pin the pieces directly.
#[test]
fn top_level_str_ignores_nested_keys_and_handles_escapes() {
    assert_eq!(
        top_level_str(r#"{"a":{"type":"inner"},"type":"outer"}"#, "type"),
        Some("outer")
    );
    assert_eq!(
        top_level_str(r#"{"list":[{"type":"x"}],"type":"outer"}"#, "type"),
        Some("outer")
    );
    // A quoted brace / escaped quote inside a value must not move
    // the depth counter or end the string early.
    assert_eq!(
        top_level_str(r#"{"text":"a \" } { b","type":"outer"}"#, "type"),
        Some("outer")
    );
    // Present but not a string, and simply absent.
    assert_eq!(top_level_str(r#"{"type":7}"#, "type"), None);
    assert_eq!(top_level_str(r#"{"other":"x"}"#, "type"), None);
}

/// A finished assistant turn, written the way claudecode writes
/// it, is `awaiting_user` — the state 0.7.38 could never produce.
#[test]
fn live_assistant_records_classify_without_being_mistaken_for_bookkeeping() {
    assert!(!is_bookkeeping_record(LIVE_ASSISTANT_TEXT));
    assert_eq!(
        activity_from_last_record(LIVE_ASSISTANT_TEXT, false),
        CcActivity::AwaitingUser
    );
    assert_eq!(
        activity_from_last_record(LIVE_ASSISTANT_TOOL_USE, false),
        CcActivity::ToolPending { executing: false }
    );
    assert!(is_bookkeeping_record(LIVE_LAST_PROMPT));
    assert!(is_bookkeeping_record(LIVE_TURN_DURATION));
}

/// Transcript text that merely QUOTES the marker (a session where
/// marspot itself is being developed does exactly this) is escaped
/// in JSON, so the unescaped pattern can't match it.
#[test]
fn a_quoted_tool_use_marker_in_message_text_is_not_a_tool_call() {
    let line = r#"{"parentUuid":"x","message":{"type":"message","role":"assistant","content":[{"type":"text","text":"we match on \"type\":\"tool_use\" here"}]},"type":"assistant"}"#;
    assert_eq!(
        activity_from_last_record(line, false),
        CcActivity::AwaitingUser
    );
}

/// The regression this whole helper exists for: claudecode appends
/// `system/turn_duration` after the assistant's closing message, so
/// the resting state hides one line up.
#[test]
fn tail_activity_skips_the_turn_duration_record_after_a_finished_turn() {
    let path = tmpfile(&format!(
        "{LIVE_TOOL_RESULT}\n{LIVE_ASSISTANT_TEXT}\n{LIVE_TURN_DURATION}\n"
    ));
    assert_eq!(
        tail_activity(&path, false),
        CcActivity::AwaitingUser,
        "a finished turn must not read as unknown just because a \
         bookkeeping record trails it"
    );
}

/// Skipping bookkeeping must not skip past a real record: a tool
/// call still pending is still pending.
#[test]
fn tail_activity_stops_at_the_first_conversation_record() {
    let path = tmpfile(&format!(
        "{LIVE_ASSISTANT_TEXT}\n{REC_TOOL_USE}\n{LIVE_TURN_DURATION}\n"
    ));
    assert_eq!(
        tail_activity(&path, true),
        CcActivity::ToolPending { executing: true }
    );
}

/// Nothing but bookkeeping in the window = no information.
#[test]
fn tail_activity_is_unknown_when_only_bookkeeping_is_visible() {
    let path = tmpfile(&format!(
        "{LIVE_TURN_DURATION}\n{LIVE_TURN_DURATION}\n"
    ));
    assert_eq!(tail_activity(&path, false), CcActivity::Unknown);
}

#[test]
fn tail_last_message_type_reads_final_record() {
    let path = tmpfile(concat!(
        r#"{"type":"mode","sessionId":"abc"}"#,
        "\n",
        r#"{"type":"user","text":"hi"}"#,
        "\n",
        r#"{"type":"assistant","text":"there"}"#,
        "\n",
    ));
    let kind = tail_last_message_type(&path).unwrap();
    assert_eq!(kind, "assistant");
}

#[test]
fn tail_last_message_type_handles_trailing_whitespace() {
    let path = tmpfile(concat!(
        r#"{"type":"user","text":"hi"}"#,
        "\n   \n",
    ));
    let kind = tail_last_message_type(&path).unwrap();
    assert_eq!(kind, "user");
}
