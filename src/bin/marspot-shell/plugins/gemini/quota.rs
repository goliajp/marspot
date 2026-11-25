//! Telling that a gemini account refused a model for the day.
//!
//! gemini writes no usage figures, so a refusal is the only reading of
//! an account.  Three shapes, all from gemini's own text:
//!
//! - an `error` record in the session file: what gemini prints when a
//!   turn stops on a quota error (the server's `QUOTA_EXHAUSTED` reason
//!   or a daily quota id, which gemini words "You have exhausted your
//!   daily quota on this model.");
//! - an `info` record "Switched to fallback model X": the model the
//!   session was on is spent for the day on this account, though the
//!   session goes on with another one;
//! - the dialog gemini puts up instead of stopping, "Usage limit reached
//!   for <model>." with "Access resets at <time>.", which is on the
//!   screen only: nothing is written to the session file while it
//!   waits for an answer.  Its paid variants (spend AI credits, or buy
//!   some) count the same, and no answer is ever typed into it.
//!
//! A refusal lasts until the time gemini gives; without one, until the
//! next midnight in the Pacific time zone, when an API key's daily
//! quota renews.

use super::model::{is_model, router_name};
#[path = "quota_clock.rs"]
mod clock;
use clock::wall_clock;
pub(crate) use clock::pacific_midnight_after;

/// Which models a refusal is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// gemini names a model.
    Model(String),
    /// "all Pro models".
    Pro,
    /// The model could not be told: the whole account, to be safe.
    Any,
}

impl Scope {
    /// A router names no model of its own: the whole account, to be safe.
    fn named(name: &str) -> Scope {
        match name.trim() {
            "all Pro models" => Scope::Pro,
            m if !is_model(m) || router_name(m).is_some() => Scope::Any,
            m => Scope::Model(m.to_string()),
        }
    }

    pub(crate) fn word(&self) -> String {
        match self {
            Scope::Model(m) => m.clone(),
            Scope::Pro => "pro*".into(),
            Scope::Any => "*".into(),
        }
    }

    pub(crate) fn from_word(w: &str) -> Scope {
        match w {
            "*" => Scope::Any,
            "pro*" => Scope::Pro,
            m => Scope::Model(m.to_string()),
        }
    }

    /// Does this refusal stop a pane on `model`?  A pane whose model is
    /// not known, or that a router picks for (`auto`, `auto-gemini-3`),
    /// is stopped by any.  Aliases are compared by the models they run.
    pub(crate) fn covers(&self, model: Option<&str>) -> bool {
        let Some(m) = model.filter(|m| router_name(m).is_none()) else { return true };
        match self {
            Scope::Any => true,
            Scope::Pro => runs(m).iter().any(|c| c.split('-').any(|w| w == "pro")),
            Scope::Model(x) => runs(x).iter().any(|a| runs(m).contains(a)),
        }
    }
}

/// The concrete models a name runs.  gemini resolves `pro` and `flash`
/// by whether preview models are turned on, which is not read here, so
/// an alias stands for each model it may resolve to.
fn runs(name: &str) -> Vec<String> {
    let name = name.to_ascii_lowercase();
    let list: &[&str] = match name.as_str() {
        "pro" => &["gemini-2.5-pro", "gemini-3-pro-preview", "gemini-3.1-pro-preview"],
        "flash" => &["gemini-2.5-flash", "gemini-3-flash-preview"],
        "flash-lite" => &["gemini-2.5-flash-lite"],
        // the tool-calling variant meters as its base model
        m => return vec![m.strip_suffix("-customtools").unwrap_or(m).to_string()],
    };
    list.iter().map(|m| m.to_string()).collect()
}

/// Where the refusal was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum How {
    Error,
    Fallback,
    /// The quota dialog with a free choice of the fallback model.
    Dialog,
    /// The dialog offering to spend AI credits.
    Overage,
    /// The dialog offering to buy AI credits.
    EmptyWallet,
}

impl How {
    pub(crate) fn word(self) -> &'static str {
        match self {
            How::Error => "error",
            How::Fallback => "fallback",
            How::Dialog => "dialog",
            How::Overage => "overage",
            How::EmptyWallet => "empty_wallet",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Refusal {
    pub at: i64,
    pub scope: Scope,
    pub resets_at: i64,
    pub how: How,
}

pub(super) const LIMIT: &str = "Usage limit reached for ";
const RESETS: &str = "Access resets at ";
pub(super) const FELL_BACK: &str = "Switched to fallback model ";

/// The model gemini names after "Usage limit reached for ", without
/// its full stop.  On a screen the row is boxed and may be wrapped: the
/// box's sides are not part of it, and a name broken at the row's end
/// goes on at the start of the next row: only when it was broken at a
/// hyphen, or is a name only with the rest, so the next row's first
/// word is never glued onto a whole name.
fn limit_scope(text: &str) -> Option<Scope> {
    let mut rows = text[text.find(LIMIT)? + LIMIT.len()..].lines().map(unboxed);
    let first = rows.next().unwrap_or("");
    if first.starts_with("all Pro") {
        return Some(Scope::Pro);
    }
    let mut words = first.split_whitespace();
    let mut name = words.next().unwrap_or("").to_string();
    if words.next().is_none() && !name.is_empty() && !name.ends_with('.') {
        let rest = rows.next().and_then(|r| r.split_whitespace().next()).unwrap_or("");
        let joined = format!("{name}{rest}");
        if name.ends_with('-') || (is_model(joined.trim_end_matches('.')) && !is_model(&name)) {
            name = joined;
        }
    }
    Some(Scope::named(name.trim_end_matches('.')))
}

/// A screen row without the box drawn around it.
pub(super) fn unboxed(row: &str) -> &str {
    row.trim().trim_matches(['│', '┃', '|']).trim()
}

/// A quota refusal in an `error` record written at `at`, on a session
/// whose model is `model` when known.  A full server (capacity) is not
/// the account's quota and does not count.
pub(crate) fn from_error(text: &str, at: i64, model: Option<&str>) -> Option<Refusal> {
    let lower = text.to_ascii_lowercase();
    let daily = text.contains("QUOTA_EXHAUSTED") || text.contains("PerDay") || lower.contains("daily quota");
    let capacity = lower.contains("capacity");
    let scope = match limit_scope(text) {
        Some(s) => s,
        None if daily && !capacity => model.map_or(Scope::Any, Scope::named),
        None => return None,
    };
    Some(Refusal { at, scope, resets_at: resets(text, at), how: How::Error })
}

/// The refusal an `info` record "Switched to fallback model X" stands
/// for: the model before it, `was`, is spent here for the day.
pub(crate) fn from_info(text: &str, at: i64, was: Option<&str>) -> Option<Refusal> {
    text.starts_with(FELL_BACK).then(|| Refusal { at, scope: was.map_or(Scope::Any, Scope::named), resets_at: pacific_midnight_after(at), how: How::Fallback })
}

/// The quota dialog, if the screen ends with one; first seen at `at`,
/// which a "reset after" in it counts from.
pub(crate) fn from_screen(screen: &str, at: i64) -> Option<Refusal> {
    let dialog = super::screen::bottom_dialog(screen)?;
    let scope = limit_scope(dialog)?;
    let how = if dialog.contains("AI Credits available") || dialog.contains("Use AI Credits") {
        How::Overage
    } else if dialog.contains("purchase more AI Credits") || dialog.contains("Get AI Credits") {
        How::EmptyWallet
    } else {
        How::Dialog
    };
    Some(Refusal { at, scope, resets_at: resets(dialog, at), how })
}

/// When what `text` refused renews, read at `at`.
fn resets(text: &str, at: i64) -> i64 {
    let after = text.to_ascii_lowercase().find("reset after ").map(|i| &text[i + "reset after ".len()..]);
    if let Some(s) = after.and_then(duration_s) {
        return at + s;
    }
    if let Some(t) = text.find(RESETS).and_then(|i| wall_clock(&text[i + RESETS.len()..], at)) {
        return t;
    }
    pacific_midnight_after(at)
}

/// `2h31m5s`, `41s`, `1h`: a duration as gemini's server words it.
fn duration_s(s: &str) -> Option<i64> {
    // the server's text, possibly inside gemini's `[API Error: …]`
    let word = s.split(|c: char| c.is_whitespace() || c == ',').next()?.trim_end_matches(['.', ']', ')']);
    let (mut total, mut num, mut any) = (0f64, String::new(), false);
    for c in word.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'h' | 'm' | 's' => {
                let n: f64 = std::mem::take(&mut num).parse().ok()?;
                total += n * match c {
                    'h' => 3600.0,
                    'm' => 60.0,
                    _ => 1.0,
                };
                any = true;
            }
            _ => return None,
        }
    }
    (any && num.is_empty()).then_some(total.ceil() as i64)
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
