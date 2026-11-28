//! `reset.status`: each account's block as devops reads it.

use marspot::agent_resets::{AccountResets, Grant as Stored, Resets};
use marspot_peer::json::Value;

use crate::plugins::claudecode::reset_claim::screen::{SessionReset, session_reset};
use super::super::ledger::Row;
use super::super::outcome::{Outcome, Res};
use super::super::status::{Session, build};

const NOW: i64 = 1_795_740_000;

fn codex() -> AccountResets {
    let g = Stored { kind: "codex_full".into(), left: Some(1), total: Some(1), starts_at: Some(1_793_478_363), ends_at: Some(1_796_070_363), ..Stored::default() };
    AccountResets { agent: "codex".into(), profile: 1, name: "Codex 1".into(), resets: Resets { grants: vec![g], ..Resets::none(NOW - 60, "app_server") } }
}

fn row(provider: &str, profile: u8, kind: &str, result: Res, at: i64, by: &str) -> Row {
    Row { at, request_id: format!("{kind}-{at}"), provider: provider.into(), profile, kind: kind.into(), by: by.into(), outcome: Outcome::of(result, "") }
}

fn said(mut r: Row, line: &str) -> Row {
    r.outcome.detail = line.into();
    r
}

/// The account's real flags (Claude Code 2.1.332, P3), switched on or off.
fn flags(enabled: bool) -> SessionReset {
    SessionReset { enabled, ..session_reset(include_str!("../../../../plugins/claudecode/reset_claim/fixtures/claude-flags.json")) }
}

fn session(profile: u8, enabled: bool, at_limit: Option<bool>) -> Session {
    Session { profile, flags: flags(enabled), at_limit }
}

fn account<'a>(v: &'a Value, provider: &str, profile: i64) -> &'a Value {
    let list = v.get("accounts").map(Value::arr).unwrap_or_default();
    let a = list.iter().find(|a| a.str_at("provider") == Some(provider) && a.get("profile").and_then(Value::i64) == Some(profile));
    a.and_then(|a| a.get("resets")).expect("the account is listed")
}

#[test]
fn reset_status_lists_each_account_with_its_session_reset_and_last_reset() {
    let sessions = [session(3, true, Some(true)), session(5, true, None), session(1, false, Some(true))];
    let used = "Session limit reset · next reset available 2026-12-02 · your weekly limit still applies";
    let rows = [
        row("codex", 1, "codex_full", Res::Reset, NOW - 3_600, "devops-ui"),
        row("codex", 1, "codex_full", Res::NotLimited, NOW - 60, "marspot-ui"),
        said(row("claude", 5, "session", Res::Reset, NOW - 86_400, "marspot-ui"), used),
    ];
    let v = build(&[codex()], &sessions, &rows, NOW);

    let c = account(&v, "codex", 1);
    assert_eq!(c.str_at("source"), Some("app_server"));
    assert_eq!(c.get("grants").map(Value::arr).map(<[Value]>::len), Some(1));
    let last = c.get("last_reset").unwrap();
    assert_eq!((last.str_at("by"), last.get("at").and_then(Value::i64)), (Some("devops-ui"), Some(NOW - 3_600)), "the last one that reset");
    assert_eq!(c.get("five_hour_reset"), Some(&Value::Null), "Codex has none");

    let p3 = account(&v, "claude", 3);
    assert_eq!(p3.str_at("source"), Some("not_read"), "no reading of its stored resets is no reading");
    let f3 = p3.get("five_hour_reset").unwrap();
    assert_eq!(f3.get("available").and_then(Value::bool), Some(true));
    assert_eq!(p3.get("last_reset"), Some(&Value::Null));

    let f5 = account(&v, "claude", 5).get("five_hour_reset").unwrap();
    assert_eq!(f5.get("available").and_then(Value::bool), Some(false), "used this week");
    assert_eq!(f5.get("next_available_at").and_then(Value::i64), marspot::cc_usage::parse_iso_utc("2026-12-02T00:00:00Z".into()), "the date the CLI named");
    assert_eq!(account(&v, "claude", 5).get("last_reset").and_then(|l| l.str_at("kind")), Some("session"));

    let f1 = account(&v, "claude", 1).get("five_hour_reset").unwrap();
    assert_eq!(f1.get("available").and_then(Value::bool), Some(false), "the CLI does not offer it there");

    let unknown = build(&[], &[session(7, true, None)], &[], NOW);
    assert_eq!(account(&unknown, "claude", 7).get("five_hour_reset").and_then(|f| f.get("available")), Some(&Value::Null), "no fresh reading: unknown");
}

#[test]
fn reset_status_the_next_session_reset_is_the_date_the_cli_named_or_unknown() {
    let at = NOW - 3_600;
    let block = |r: Row| {
        let v = build(&[], &[session(4, true, Some(true))], &[r], NOW);
        account(&v, "claude", 4).get("five_hour_reset").cloned().unwrap()
    };
    let spent = block(said(row("claude", 4, "session", Res::AlreadyUsed, at, "devops-ui"), "Weekly reset used · available again Dec 4"));
    assert_eq!(spent.get("next_available_at").and_then(Value::i64), marspot::cc_usage::parse_iso_utc("2026-12-04T00:00:00Z".into()), "a day with no year is the next such day");
    assert_eq!(spent.get("available").and_then(Value::bool), Some(false));

    let mute = block(said(row("claude", 4, "session", Res::AlreadyUsed, at, "devops-ui"), "Weekly reset used"));
    assert_eq!(mute.get("next_available_at"), Some(&Value::Null), "never a week after the use");
    assert_eq!(mute.get("available"), Some(&Value::Null), "used this week, offered again when nobody said");

    let past = block(said(row("claude", 4, "session", Res::Reset, NOW - 8 * 86_400, "devops-ui"), "Session limit reset · next reset available 2026-11-25 · your weekly limit still applies"));
    assert_eq!(past.get("next_available_at"), Some(&Value::Null), "that date has come");
    assert_eq!(past.get("available").and_then(Value::bool), Some(true), "from the fresh five-hour reading");
}
