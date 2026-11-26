//! What the Claude CLI shows around `/limit-reset`, read as text: the
//! account's cached feature flags that switch the command on and give
//! its lines, the row of a listed grant that matches the confirmed one,
//! and the line that says how the claim went.

use marspot_peer::json::{self, Value};

use super::{Grant, Res};
use crate::plugins::claudecode::limit_reset_screen;

/// What the CLI printed on an account not at its five-hour limit
/// (Claude Code 2.1.332, 2026-11-26).
pub(super) const NOT_AVAILABLE: &str = "A 5-hour limit reset isn't available right now.";

/// The weekly session reset as the account's cached flags describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SessionReset {
    pub enabled: bool,
    /// Each with `{date}` where the CLI puts a date.
    pub success: Option<String>,
    pub spent: Option<String>,
    pub unavailable: Option<String>,
}

fn flag<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    ["cachedGrowthBookFeatures", "cachedFeatureFlags"].iter().find_map(|k| v.at(&[k, name]))
}

/// The weekly session reset of the account whose `.claude.json` is
/// `body`; read only, never written.
pub(super) fn session_reset(body: &str) -> SessionReset {
    let Ok(v) = json::parse(body) else { return SessionReset::default() };
    let Some(f) = flag(&v, "tengu_nifty_lemur") else { return SessionReset::default() };
    let line = |k: &str| f.str_at(k).map(str::to_string);
    SessionReset {
        enabled: f.get("enabled").and_then(Value::bool) == Some(true),
        success: line("successLine"),
        spent: line("spentLine"),
        unavailable: line("unavailableLine"),
    }
}

/// Whether the CLI lists stored resets on that account at all.
pub(super) fn lists_grants(body: &str) -> bool {
    let Ok(v) = json::parse(body) else { return false };
    match flag(&v, "tengu_cedar_ember") {
        Some(Value::Bool(b)) => *b,
        Some(f) => f.get("enabled").and_then(Value::bool) == Some(true),
        None => false,
    }
}

/// Whether `line` is `template` with something in place of `{date}`.
fn fits(line: &str, template: &str) -> bool {
    match template.split_once("{date}") {
        Some((head, tail)) => line.contains(head.trim()) && line.contains(tail.trim()),
        None => line.contains(template.trim()),
    }
}

/// How the claim went, from the screen after it, and the line that says
/// so; `None` while no line says.
pub(super) fn result_of(screen: &str, s: &SessionReset) -> Option<(Res, String)> {
    for line in screen.lines().rev().map(str::trim).filter(|l| !l.is_empty()) {
        let lower = line.to_ascii_lowercase();
        let by = |t: &Option<String>| t.as_deref().is_some_and(|t| fits(line, t));
        let result = if by(&s.success) || lower.contains("limits reset") || (lower.contains("reset used ·") && lower.contains("back in your weekly")) {
            Res::Reset
        } else if by(&s.spent) || lower.contains("already used") {
            Res::AlreadyUsed
        } else if line.contains(NOT_AVAILABLE) || by(&s.unavailable) {
            Res::Unavailable
        } else if lower.contains("cooldown") || lower.contains("cooling down") {
            Res::Cooldown
        } else if lower.contains("not eligible") || lower.contains("ineligible") {
            Res::Ineligible
        } else if lower.contains("unknown command") || lower.contains("unknown slash command") {
            Res::Unavailable
        } else {
            continue;
        };
        return Some((result, line.to_string()));
    }
    None
}

/// Two times this close are the same: the screen shows minutes.
const SAME_TIME_S: i64 = 120;

/// The row of the screen's grant list that is the confirmed grant: same
/// kind, same count left and the same end where both say.  `None` when
/// the screen lists no such row, or more than one.
pub(super) fn row_for(screen: &str, g: &Grant, now: i64) -> Option<usize> {
    let listed = limit_reset_screen::read(screen, now).grants;
    let same = |x: &marspot::agent_resets::Grant| {
        let ends = match (x.ends_at, g.ends_at) {
            (Some(a), Some(b)) => (a - b).abs() <= SAME_TIME_S,
            _ => true,
        };
        x.kind == g.kind && (g.left.is_none() || x.left == g.left) && ends
    };
    let mut rows = listed.iter().enumerate().filter(|(_, x)| same(x)).map(|(i, _)| i);
    let row = rows.next()?;
    rows.next().is_none().then_some(row)
}

/// Whether the screen lists grants to choose from: rows of grants, and
/// none of the session reset's own lines, which name a kind too.
pub(super) fn lists(screen: &str, s: &SessionReset, now: i64) -> bool {
    let session_line = |l: &str| {
        l.contains(NOT_AVAILABLE) || [&s.success, &s.spent, &s.unavailable].iter().any(|t| t.as_deref().is_some_and(|t| fits(l, t)))
    };
    !screen.lines().any(session_line) && !limit_reset_screen::read(screen, now).grants.is_empty()
}

/// Whether the screen asks to confirm what was chosen.
pub(super) fn asks_to_confirm(screen: &str) -> bool {
    let lower = screen.to_ascii_lowercase();
    ["confirm", "are you sure", "yes, use"].iter().any(|w| lower.contains(w))
}
