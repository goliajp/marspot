//! What the Claude CLI shows around `/limit-reset`, read against the
//! account's real cached flags (Claude Code 2.1.332, P3, 2026-11-27)
//! and the one line the CLI was seen to print; and that a claim of any
//! kind but the session reset types nothing.

use std::time::{Duration, Instant};

use super::super::claude_screen::{NOT_AVAILABLE, next_offered, result_of, session_reset};
use super::super::{Grant, Res, claude};

const FLAGS: &str = include_str!("../fixtures/claude-flags.json");

/// 2026-11-27T00:00:00Z.
const NOW: i64 = 1_795_737_600;

fn screen(line: &str) -> String {
    format!("╭────────╮\n│ > /limit-reset │\n╰────────╯\n  ⎿  {line}\n\n  ? for shortcuts\n")
}

#[test]
fn reset_screen_the_session_reset_and_its_lines_come_from_the_cached_flags() {
    let s = session_reset(FLAGS);
    assert!(s.enabled, "the growth-book copy is the one the CLI uses");
    assert_eq!(s.success.as_deref(), Some("Session limit reset · next reset available {date} · your weekly limit still applies"));
    assert!(!session_reset("{}").enabled && !session_reset("not json").enabled);
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
fn reset_screen_only_what_follows_the_echoed_command_is_its_answer() {
    let s = session_reset(FLAGS);
    let earlier = format!("  ⎿  Limits reset\n  ⎿  you already used that\n{}", screen("Thinking…"));
    assert_eq!(result_of(&earlier, &s), None, "words above the echo are an older answer, or anything else");
    assert_eq!(result_of("  ⎿  Limits reset\n\n  ? for shortcuts\n", &s), None, "no echo: nothing is read");
    let twice = format!("{}{}", screen("Weekly reset used · available again Dec 4"), screen(NOT_AVAILABLE));
    assert_eq!(result_of(&twice, &s).map(|r| r.0), Some(Res::Unavailable), "the last echo counts");
}

#[test]
fn reset_screen_the_next_offer_is_the_date_the_line_names() {
    let s = session_reset(FLAGS);
    let day = |d: &str| marspot::cc_usage::parse_iso_utc(format!("{d}T00:00:00Z"));
    let ok = "Session limit reset · next reset available Dec 4 · your weekly limit still applies";
    assert_eq!(next_offered(ok, &s, NOW), day("2026-12-04"));
    assert_eq!(next_offered("Weekly reset used · available again Jan 2", &s, NOW), day("2027-01-02"), "past the year's end");
    assert_eq!(next_offered("Weekly reset used · available again 2026-12-03", &s, NOW), day("2026-12-03"));
    assert_eq!(next_offered("Weekly reset used · available again December 3, 2026", &s, NOW), day("2026-12-03"));
    assert_eq!(next_offered("Weekly reset used · available again soon", &s, NOW), None);
    assert_eq!(next_offered(NOT_AVAILABLE, &s, NOW), None);
}

#[test]
fn reset_screen_a_claim_of_a_stored_claude_grant_types_nothing() {
    // the chooser behind tengu_cedar_ember has never been seen on a real
    // screen: those kinds are refused before any pane is opened, which
    // here would fail on the missing control socket instead
    for kind in ["full", "weekly", "fable", "codex_full"] {
        let g = Grant { kind: kind.into(), grant_ref: "claude:3:g1".into(), ..Grant::default() };
        let o = claude::claim(3, &g, Instant::now() + Duration::from_secs(5));
        assert_eq!(o.result, Res::Unavailable, "{kind}: {o:?}");
        assert!(o.detail.contains("session reset"), "{o:?}");
    }
}
