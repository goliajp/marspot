//! Reading a quota refusal out of a transcript: which wording counts, and when.

use super::*;

/// The record a real refusal makes, copied off disk rather than
/// imagined: `isApiErrorMessage` with the CLI's own text and the
/// instant it happened.
const REFUSAL: &str = concat!(
    r#"{"parentUuid":"8f670e3e","isSidechain":false,"type":"assistant","#,
    r#""isApiErrorMessage":true,"timestamp":"2026-10-01T11:51:52.440Z","#,
    r#""message":{"role":"assistant","content":[{"type":"text","text":"#,
    r#""You've reached your Fable limit. Run /usage-credits to continue "#,
    r#"or switch models with /model."}]}}"#
);

/// A refusal is in the tail window at the instant it is written
/// and gone from it soon after, so it has to be remembered.
///
/// Measured on the real file this was written for: thirty-two
/// kilobytes of a 21.8 MB transcript reached back six minutes, and
/// the refusals that stranded the pane were outside it twenty-five
/// minutes later. A scan that re-derives this every time answers
/// "no refusal" for a pane that is still shut out -- which is the
/// same outcome as not having built any of this.
#[test]
fn a_refusal_that_has_scrolled_out_of_the_window_is_still_remembered() {
    let tmp = TempDir::new("refusal-window");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("session.jsonl");

    let mut ctx = ctx_for_test(dir.clone());
    // The moment it happens: the record is the newest line.
    fs::write(&path, format!("{REFUSAL}\n")).unwrap();
    assert_eq!(
        ctx.remember_refusal(&path, &tail_window(&path)).map(|r| r.at),
        Some(1_790_855_512),
        "seen when it is written"
    );

    // And later: the session keeps talking until the record is
    // past the window.
    let filler = format!("{}\n", r#"{"type":"assistant","timestamp":"2026-10-01T12:30:00.000Z"}"#);
    let mut body = format!("{REFUSAL}\n");
    while body.len() < 80 * 1024 {
        body.push_str(&filler);
    }
    fs::write(&path, &body).unwrap();
    let window = tail_window(&path);
    assert_eq!(
        last_quota_refusal(&window),
        None,
        "the fixture has to actually push it out, or this proves nothing"
    );
    assert_eq!(
        ctx.remember_refusal(&path, &window).map(|r| r.at),
        Some(1_790_855_512),
        "but the pane is no less shut out than it was"
    );
}

#[test]
fn a_refusal_in_the_transcript_is_read_with_its_time() {
    let r = last_quota_refusal(REFUSAL).expect("the record says when");
    assert_eq!(r.at, 1_790_855_512, "2026-10-01T11:51:52Z");
    assert_eq!(r.model, "fable", "and which model it is about");
}

/// The newest one wins: a transcript holds every refusal it ever
/// had, and what matters is whether the account said no *since*
/// the feed last looked.
#[test]
fn the_newest_refusal_is_the_one_that_counts() {
    let older = REFUSAL.replace("11:51:52", "09:18:13");
    let tail = format!("{older}\n{REFUSAL}\n");
    assert_eq!(last_quota_refusal(&tail).map(|r| r.at), Some(1_790_855_512));
}

/// Being overloaded is not being out of quota, and moving a pane
/// for it restarts a session for nothing. The HTTP status and the
/// error type are shared between the two, so neither is matched.
/// The transcript follows a session across a switch; the refusal
/// the old account gave stays in it and is not the new one's.
#[test]
fn only_a_refusal_after_this_claude_started_counts() {
    let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_002_529);
    let at = |at| Some(quota::Refusal { at, model: "fable".into(), logged_out: false, window: quota::Window::Week });
    assert_eq!(refused_since(at(1_790_966_543), started), None, "the day before");
    assert_eq!(refused_since(at(1_791_002_600), started), at(1_791_002_600));
    assert_eq!(refused_since(None, started), None);
}

#[test]
fn a_busy_server_is_not_a_spent_account() {
    let overloaded = REFUSAL.replace(
        "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
        "API Error: 429 overloaded_error · Retrying in 8 seconds",
    );
    assert_eq!(
        last_quota_refusal(&overloaded),
        None,
        "a retryable error is not a reason to move the pane"
    );
}

/// And a record that is not flagged as an API error does not
/// count, however it reads -- a transcript that quotes the message
/// is not a transcript that received it. (This one is written from
/// a session that was discussing the bug: the text was in four
/// profiles' transcripts before any of them had been refused.)
#[test]
fn quoting_the_message_is_not_receiving_it() {
    let quoted = REFUSAL.replace(r#""isApiErrorMessage":true,"#, "");
    assert_eq!(last_quota_refusal(&quoted), None);
}

#[test]
fn the_credits_wording_counts_too() {
    let credits = REFUSAL.replace(
        "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
        "Opus 5.5 requires usage credits.",
    );
    assert_eq!(last_quota_refusal(&credits).map(|r| r.model), Some("opus-5-5".into()));
}

/// Running out of the account's own windows names no model, so it
/// covers every pane. These went unread until 2026-10-31: 389
/// session and 103 weekly refusals sat in the transcripts.
#[test]
fn the_account_window_wording_counts_for_every_model() {
    for text in [
        "You've hit your session limit · resets 4:50pm (Asia/Tokyo)",
        "You've hit your weekly limit · resets Oct 14 at 2pm (Asia/Tokyo)",
    ] {
        let line = REFUSAL.replace(
            "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
            text,
        );
        let r = last_quota_refusal(&line).expect(text);
        assert!(r.model.is_empty(), "{text}: names no model");
        assert!(!r.logged_out);
    }
    let fast = REFUSAL.replace(
        "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
        "You've hit your fast limit",
    );
    assert_eq!(last_quota_refusal(&fast), None, "fast mode is not the account");
}

/// The record the CLI writes when it cannot log in, as it was
/// written on 2026-09-28 (trimmed).
const LOGIN_EXPIRED: &str = concat!(
    r#"{"parentUuid":"18fba766","isSidechain":false,"type":"assistant","#,
    r#""timestamp":"2026-09-28T08:41:45.587Z","message":{"model":"<synthetic>","#,
    r#""role":"assistant","content":[{"type":"text","text":"Login expired · Please run /login"}]},"#,
    r#""error":"authentication_failed","isApiErrorMessage":true}"#,
);

#[test]
fn a_failed_login_is_a_refusal_for_every_model() {
    let r = last_quota_refusal(LOGIN_EXPIRED).expect("a failed login shuts the pane out");
    assert_eq!(r.at, 1_790_584_905, "2026-09-28T08:41:45Z");
    assert!(r.logged_out);
    assert!(r.model.is_empty(), "it names no model, so it covers them all");
    assert!(!last_quota_refusal(REFUSAL).unwrap().logged_out, "running out is not being logged out");
    let quoted = LOGIN_EXPIRED.replace(r#","isApiErrorMessage":true"#, "");
    assert_eq!(last_quota_refusal(&quoted), None, "only a record the CLI flagged");
}
