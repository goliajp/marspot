//! `reset.status`: each account's block as devops reads it.

use marspot::agent_resets::{AccountResets, Grant as Stored, Resets};
use marspot_peer::json::Value;

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

fn account<'a>(v: &'a Value, provider: &str, profile: i64) -> &'a Value {
    let list = v.get("accounts").map(Value::arr).unwrap_or_default();
    let a = list.iter().find(|a| a.str_at("provider") == Some(provider) && a.get("profile").and_then(Value::i64) == Some(profile));
    a.and_then(|a| a.get("resets")).expect("the account is listed")
}

#[test]
fn reset_status_lists_each_account_with_its_session_reset_and_last_reset() {
    let sessions = [
        Session { profile: 3, enabled: true, at_limit: Some(true) },
        Session { profile: 5, enabled: true, at_limit: None },
        Session { profile: 1, enabled: false, at_limit: Some(true) },
    ];
    let rows = [
        row("codex", 1, "codex_full", Res::Reset, NOW - 3_600, "devops-ui"),
        row("codex", 1, "codex_full", Res::NotLimited, NOW - 60, "marspot-ui"),
        row("claude", 5, "session", Res::Reset, NOW - 86_400, "marspot-ui"),
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
    assert_eq!(f5.get("next_available_at").and_then(Value::i64), Some(NOW - 86_400 + 7 * 86_400));
    assert_eq!(account(&v, "claude", 5).get("last_reset").and_then(|l| l.str_at("kind")), Some("session"));

    let f1 = account(&v, "claude", 1).get("five_hour_reset").unwrap();
    assert_eq!(f1.get("available").and_then(Value::bool), Some(false), "the CLI does not offer it there");

    let unknown = build(&[], &[Session { profile: 7, enabled: true, at_limit: None }], &[], NOW);
    assert_eq!(account(&unknown, "claude", 7).get("five_hour_reset").and_then(|f| f.get("available")), Some(&Value::Null), "no fresh reading: unknown");
}
