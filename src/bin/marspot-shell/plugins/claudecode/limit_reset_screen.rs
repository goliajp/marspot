//! What the CLI's own `/limit-reset` screen lists, read from the text
//! of a pane: one grant per line that names a kind of reset.
//!
//! Only reading: the screen is left with Esc, nothing on it is chosen.
//! The words matched here are written against the fields the CLI is
//! known to keep for a grant (its kind, how many are left of how many,
//! when it starts and ends, whether it can be used now); they are to be
//! checked against a screen captured from the real CLI, which the
//! fixture in the tests stands in for until then.

use marspot::agent_resets::{Grant, Outcome, Resets};

/// Words of a screen that says the command does not exist here.
const MISSING: [&str; 3] = ["unknown command", "unknown slash command", "no such command"];
/// Words of a screen that says the account cannot have resets.
const INELIGIBLE: [&str; 3] = ["not eligible", "ineligible", "isn't available for your"];

/// The reading the screen `text` gives, as of `now`.
pub(super) fn read(text: &str, now: i64) -> Resets {
    let lower = text.to_ascii_lowercase();
    let mut r = Resets::none(now, "limit_reset_screen");
    if MISSING.iter().any(|w| lower.contains(w)) {
        r.outcome = Outcome::CommandMissing;
        return r;
    }
    if INELIGIBLE.iter().any(|w| lower.contains(w)) {
        r.outcome = Outcome::Ineligible;
        r.eligible = Some(false);
        r.ineligible_reason = ["surface", "plan", "region"].iter().find(|w| lower.contains(*w)).map(|w| w.to_string());
        return r;
    }
    r.eligible = Some(true);
    r.grants = lower.lines().filter_map(|l| grant(l, now)).collect();
    r
}

/// The kind a line's label names, and what that kind brings back.
fn kind(line: &str) -> Option<(&'static str, &'static [&'static str])> {
    let has = |w: &str| line.contains(w);
    if has("fable") {
        Some(("fable", &["seven_day_overage_included"]))
    } else if has("weekly") || has("week reset") {
        Some(("weekly", &["seven_day"]))
    } else if has("session") || has("5-hour") || has("5 hour") || has("5h") {
        Some(("session", &["five_hour"]))
    } else if has("full") || has("all limits") || has("limits reset") {
        Some(("full", &["five_hour", "seven_day"]))
    } else {
        None
    }
}

fn grant(line: &str, now: i64) -> Option<Grant> {
    let line = line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let (kind, restores) = kind(line)?;
    let (left, total) = counts(line);
    let needs_limit = ["at your limit", "when you hit", "once you reach", "needs limit"].iter().any(|w| line.contains(w));
    let later = ["available after", "not yet", "starts in", "cooldown"].iter().any(|w| line.contains(w));
    let now_ok = ["available now", "usable now", "ready"].iter().any(|w| line.contains(w));
    Some(Grant {
        kind: kind.into(),
        restores: restores.iter().map(|w| w.to_string()).collect(),
        gives_back_weekly: kind == "fable",
        left,
        total,
        usable_now: if later { Some(false) } else if now_ok { Some(true) } else { None },
        use_requires_limit: needs_limit.then_some(true),
        unavailable_reason: if needs_limit && later { Some("needs_limit".into()) } else if later { Some("cooldown".into()) } else { None },
        starts_at: after_word(line, &["from ", "starts ", "available after "], now),
        ends_at: after_word(line, &["until ", "expires ", "ends ", "through "], now),
        ..Grant::default()
    })
}

/// `2 of 3`, `2/3` or `2 left`.
fn counts(line: &str) -> (Option<u32>, Option<u32>) {
    let words: Vec<&str> = line.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if let Some((a, b)) = w.split_once('/')
            && let (Ok(a), Ok(b)) = (a.parse(), b.trim_end_matches(|c: char| !c.is_ascii_digit()).parse())
        {
            return (Some(a), Some(b));
        }
        let Ok(n) = w.parse::<u32>() else { continue };
        match (words.get(i + 1), words.get(i + 2).and_then(|t| t.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok())) {
            (Some(&"of"), Some(total)) => return (Some(n), Some(total)),
            (Some(next), _) if next.starts_with("left") || next.starts_with("remaining") => return (Some(n), None),
            _ => {}
        }
    }
    (None, None)
}

/// The time written after the first of `words` in `line`.
fn after_word(line: &str, words: &[&str], now: i64) -> Option<i64> {
    words.iter().find_map(|w| line.find(w).and_then(|i| time_in(&line[i + w.len()..], now)))
}

/// A time at the start of `s`: `2026-12-01`, `2026-12-01 14:30` (UTC),
/// or `in 3 days` / `in 5h`.
fn time_in(s: &str, now: i64) -> Option<i64> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix("in ") {
        let mut it = rest.split_whitespace();
        let first = it.next()?;
        let digits = first.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let n: i64 = digits.parse().ok()?;
        let unit = if digits.len() < first.len() { &first[digits.len()..] } else { it.next()? };
        let secs = match unit.chars().next()? {
            'm' if unit.starts_with("mo") => return None,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 7 * 86_400,
            _ => return None,
        };
        return Some(now + n * secs);
    }
    let date = s.get(..10)?;
    let iso = match s.get(10..16) {
        Some(t) if t.starts_with([' ', 't']) && t.as_bytes()[3] == b':' => format!("{date}T{}:00+00:00", &t[1..]),
        _ => format!("{date}T00:00:00+00:00"),
    };
    marspot::cc_usage::parse_iso_utc(iso)
}

#[cfg(test)]
#[path = "limit_reset_screen_tests.rs"]
mod tests;
