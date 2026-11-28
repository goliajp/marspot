//! `reset.status`: every Claude and Codex account's resets block in the
//! shape devops reads, from marspot's own readings, with the weekly
//! session reset and the last reset marspot used filled in.  devops
//! pulls this; marspot never sends it anywhere.

use marspot::agent_resets::{AccountResets, Resets};
use marspot_peer::json::{Value, obj};

use super::claude_screen::{self, SessionReset};
use super::ledger::Row;

/// A five-hour reading older than this does not say the account is at
/// its limit now.
const FRESH_S: i64 = 10 * 60;
/// A use of the session reset older than this is not this week's.
const WEEK_S: i64 = 7 * 86_400;

/// What is known of one Claude account's weekly session reset.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Session {
    pub profile: u8,
    /// Whether the CLI offers it on this account and the lines it
    /// prints, from its cached flags.
    pub flags: SessionReset,
    /// The five-hour window is spent, from a fresh reading; `None` when
    /// none is fresh.
    pub at_limit: Option<bool>,
}

/// The block of one account.  A use this week with no date the CLI
/// named leaves both the next date and whether it is offered unknown.
fn session_block(s: &Session, rows: &[&Row], now: i64) -> Value {
    let used = rows.iter().rev().find(|r| r.kind == "session" && matches!(r.outcome.result.as_str(), "reset" | "already_used"));
    let named = used.and_then(|r| claude_screen::next_offered(&r.outcome.detail, &s.flags, r.at));
    let next = named.filter(|t| *t > now);
    let this_week = used.is_some_and(|r| now - r.at < WEEK_S);
    let available = match (s.flags.enabled, next, named, this_week) {
        (false, ..) | (_, Some(_), _, _) => Some(false),
        (true, None, None, true) => None,
        (true, None, _, _) => s.at_limit,
    };
    obj([
        ("available", available.map_or(Value::Null, Value::from)),
        ("next_available_at", next.map_or(Value::Null, Value::from)),
        ("uses_weekly", true.into()),
        ("enabled", s.flags.enabled.into()),
    ])
}

fn last_reset(rows: &[&Row]) -> Value {
    let Some(r) = rows.iter().rev().find(|r| r.outcome.result.as_str() == "reset") else { return Value::Null };
    obj([
        ("at", r.at.into()),
        ("kind", r.kind.as_str().into()),
        ("by", r.by.as_str().into()),
        ("request_id", r.request_id.as_str().into()),
        ("result", "reset".into()),
    ])
}

/// The answer, from the readings `accounts`, the Claude accounts'
/// `sessions` and the claim facts `rows`.  A Claude account with no
/// reading of its stored resets says `not_read`: nobody looked.
pub(super) fn build(accounts: &[AccountResets], sessions: &[Session], rows: &[Row], now: i64) -> Value {
    let mut out: Vec<(String, u8, String, Resets)> = accounts
        .iter()
        .filter(|a| matches!(a.agent.as_str(), "claude" | "codex"))
        .map(|a| (a.agent.clone(), a.profile, a.name.clone(), a.resets.clone()))
        .collect();
    for s in sessions {
        if !out.iter().any(|(a, p, _, _)| a == "claude" && *p == s.profile) {
            let name = if s.profile == 0 { "Claude".into() } else { format!("Claude {}", s.profile) };
            out.push(("claude".into(), s.profile, name, Resets::none(now, "not_read")));
        }
    }
    out.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let list = out
        .into_iter()
        .map(|(agent, profile, name, r)| {
            let mine: Vec<&Row> = rows.iter().filter(|x| x.provider == agent && x.profile == profile).collect();
            let mut block = r.to_value(&format!("{agent}:{profile}"));
            if let Some(s) = sessions.iter().find(|s| agent == "claude" && s.profile == profile) {
                block.set("five_hour_reset", session_block(s, &mine, now));
            }
            block.set("last_reset", last_reset(&mine));
            obj([("provider", agent.into()), ("profile", i64::from(profile).into()), ("name", name.into()), ("resets", block)])
        })
        .collect();
    obj([("generated_at", now.into()), ("accounts", Value::Arr(list))])
}

/// The session reset facts of every Claude account on this machine.
fn sessions(now: i64) -> Vec<Session> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let usage = marspot::cc_usage::read();
    crate::plugins::claudecode::discover_profiles()
        .into_iter()
        .map(|profile| {
            let flags = std::fs::read_to_string(home.join(format!(".claude-profile-{profile}/.claude.json"))).unwrap_or_default();
            let name = format!("Claude {profile}");
            let row = usage.as_ref().and_then(|u| u.accounts.iter().find(|a| a.name == name));
            let at_limit = row.filter(|a| now - a.collected_at <= FRESH_S).and_then(|a| a.util_5h).map(|u| u >= 1.0);
            Session { profile, flags: claude_screen::session_reset(&flags), at_limit }
        })
        .collect()
}

pub(crate) fn status() -> Value {
    let now = super::unix_now();
    let rows: Vec<Row> = super::ledger().lock().unwrap_or_else(|p| p.into_inner()).rows().cloned().collect();
    build(&marspot::agent_resets::read(now), &sessions(now), &rows, now)
}
