//! What the Claude CLI shows around `/limit-reset`, read against the
//! account's real cached flags (Claude Code 2.1.332, P3, 2026-11-27)
//! and the one line the CLI was seen to print, plus the stand-in grant
//! list the screen parser is written against.

use super::super::claude_screen::{NOT_AVAILABLE, asks_to_confirm, lists, lists_grants, result_of, row_for, session_reset};
use super::super::{Grant, Res};

const FLAGS: &str = include_str!("../fixtures/claude-flags.json");

/// 2026-11-27T00:00:00Z.
const NOW: i64 = 1_795_737_600;

/// The stand-in list of the screen parser's own tests.
const LIST: &str = "\
 Limit resets

 ❯ Full reset        2 of 3 left   available now   until 2026-12-01 14:30
   Fable reset       1/1           at your limit   from 2026-11-28 until 2026-12-05
   Weekly reset      1 left        available after cooldown   expires in 3 days

 Esc to go back
";

fn screen(line: &str) -> String {
    format!("╭────────╮\n│ > /limit-reset │\n╰────────╯\n  ⎿  {line}\n\n  ? for shortcuts\n")
}

#[test]
fn reset_screen_the_session_reset_and_its_lines_come_from_the_cached_flags() {
    let s = session_reset(FLAGS);
    assert!(s.enabled, "the growth-book copy is the one the CLI uses");
    assert_eq!(s.success.as_deref(), Some("Session limit reset · next reset available {date} · your weekly limit still applies"));
    assert!(!lists_grants(FLAGS), "no stored-reset list on this account");
    assert!(!session_reset("{}").enabled && !session_reset("not json").enabled);
    assert!(lists_grants(r#"{"cachedGrowthBookFeatures":{"tengu_cedar_ember":{"enabled":true}}}"#));
}

#[test]
fn reset_screen_each_line_the_cli_prints_is_its_result() {
    let s = session_reset(FLAGS);
    let cases = [
        (NOT_AVAILABLE.to_string(), Res::Unavailable),
        ("Session limit reset · next reset available Dec 4 · your weekly limit still applies".into(), Res::Reset),
        ("Weekly reset used · available again Dec 4".into(), Res::AlreadyUsed),
        ("Couldn't reset your session limit right now · try again in a moment".into(), Res::Unavailable),
        ("Limits reset".into(), Res::Reset),
        ("Fable reset used · what Fable used is back in your weekly limit".into(), Res::Reset),
        ("Unknown slash command: limit-reset".into(), Res::Unavailable),
    ];
    for (line, want) in cases {
        let (got, said) = result_of(&screen(&line), &s).unwrap_or_else(|| panic!("{line}"));
        assert_eq!(got, want, "{line}");
        assert!(said.ends_with(line.as_str()), "only the result line: {said}");
    }
    assert_eq!(result_of(&screen("Thinking…"), &s), None);
    assert_eq!(result_of("? for shortcuts\n/limit-reset to reset your session limit now · uses weekly limit · 1/week", &s), None, "the notice is no result");
}

#[test]
fn reset_screen_a_session_line_is_never_read_as_a_list() {
    let s = session_reset(FLAGS);
    assert!(lists(LIST, &s, NOW));
    assert!(!lists(&screen(NOT_AVAILABLE), &s, NOW), "it names a 5-hour reset but lists nothing");
    assert!(!lists(&screen("Session limit reset · next reset available Dec 4 · your weekly limit still applies"), &s, NOW));
}

#[test]
fn reset_screen_only_the_one_matching_row_is_chosen() {
    let fable = Grant { kind: "fable".into(), grant_ref: "claude:3:g2".into(), left: Some(1), ends_at: marspot::cc_usage::parse_iso_utc("2026-12-05T00:00:00Z".into()), starts_at: None };
    assert_eq!(row_for(LIST, &fable, NOW), Some(1));
    let full = Grant { kind: "full".into(), left: Some(2), ends_at: marspot::cc_usage::parse_iso_utc("2026-12-01T14:31:00Z".into()), ..fable.clone() };
    assert_eq!(row_for(LIST, &full, NOW), Some(0), "a minute off is the same end");
    assert_eq!(row_for(LIST, &Grant { left: Some(1), ..full.clone() }, NOW), None, "a different count left");
    assert_eq!(row_for(LIST, &Grant { ends_at: Some(NOW), ..full.clone() }, NOW), None, "a different end");
    assert_eq!(row_for(LIST, &Grant { kind: "session".into(), ..full }, NOW), None);
    let twice = LIST.replace("   Weekly reset ", "   Fable reset  ").replace("expires in 3 days", "until 2026-12-05");
    assert_eq!(row_for(&twice, &Grant { left: None, ..fable }, NOW), None, "two rows alike: nothing is chosen");
    assert!(asks_to_confirm("Use 1 fable reset now?\n❯ Yes, use it\n  No"));
    assert!(!asks_to_confirm(LIST));
}
