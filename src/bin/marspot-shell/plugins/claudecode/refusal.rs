//! What a pane learns for itself about its account, from the records
//! the CLI writes into its transcript: that it ran out, or that it
//! could not log in.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use super::model::short_model;
use super::quota::{same_model, Room};
use super::transcript::{record_type, top_level_str};
use super::worker::WorkerCtx;

/// When this session was last told by the API that it had reached a
/// limit, as unix seconds.
///
/// Reads the tail window the scan already has, so it costs nothing
/// beyond the scan of those lines.
///
/// Claude Code records the refusal itself rather than leaving it on
/// the screen: `"isApiErrorMessage":true` on an assistant record whose
/// text is the limit message, with the timestamp it happened. That is
/// what makes this evidence and not a screen-scrape -- and it is the
/// only timely evidence there is, because the usage feed is collected
/// behind what a session is actually spending.
///
/// Three texts are matched, all read out of the CLI's own templates
/// rather than guessed: the model-limit branch, the credits branch,
/// and the account-window branch ("You've hit your session limit",
/// "… weekly limit"), which names no model.
/// Deliberately not `rate_limit` or `429`, which a loaded server
/// produces too -- being overloaded is not being out of quota, and
/// moving a pane for it costs a restart for nothing.
///
/// A failed login counts as well: the CLI flags it with its own error
/// code, `authentication_failed`, whichever of its wordings ("Login
/// expired · Please run /login", "OAuth token revoked · …") it shows.
pub(super) fn last_quota_refusal(tail: &str) -> Option<Refusal> {
    let line = tail.lines().rev().find(|l| {
        l.contains("\"isApiErrorMessage\":true") && (says_out_of_quota(l) || says_logged_out(l))
    })?;
    let logged_out = says_logged_out(line);
    let five_hour = ACCOUNT_WINDOWS[..2].iter().any(|w| line.contains(w));
    Some(Refusal {
        at: record_timestamp(line)?,
        model: if logged_out { String::new() } else { refused_model(line) },
        logged_out,
        window: if five_hour { Window::FiveHour } else { Window::Week },
    })
}

impl WorkerCtx {
    /// The newest refusal known for this transcript: what the window
    /// shows now, or what it showed when it was still in there.
    ///
    /// Kept rather than re-derived because the window moves -- see
    /// `last_refusal` for how fast.
    pub(super) fn remember_refusal(&mut self, path: &PathBuf, window: &str) -> Option<Refusal> {
        if let Some(seen) = last_quota_refusal(window) {
            let kept = self.last_refusal.entry(path.clone()).or_insert_with(|| seen.clone());
            if seen.at > kept.at {
                *kept = seen;
            }
        }
        self.last_refusal.get(path).cloned()
    }
}

/// The quota refusal a background agent of this session ended on, when
/// that is the newest thing any of its background agents reported.
///
/// An agent runs inside claude and writes its own transcript; what the
/// session's transcript gets is the notice the CLI queues when one
/// stops: `<status>failed</status>` and, for an API error, the CLI's
/// text for it with the model the request named. Only that queued
/// record is read, never the same words quoted in a message.
///
/// A later notice from an agent that did not end this way -- finished,
/// or waiting on work of its own -- is that work making progress, and
/// then there is nothing to report. The model is empty when the notice
/// names none.
pub(super) fn last_agent_refusal(tail: &str) -> Option<Refusal> {
    let (line, notice) = tail.lines().rev().find_map(|l| Some((l, agent_notice(l)?)))?;
    if !(notice.contains("<status>failed</status>") && says_out_of_quota(notice)) {
        return None;
    }
    const SENT: &str = "model sent to the API: ";
    let model = notice.find(SENT).map(|i| &notice[i + SENT.len()..]).map_or(String::new(), |rest| {
        short_model(&rest[..rest.find(')').unwrap_or(rest.len())])
    });
    let five_hour = ACCOUNT_WINDOWS[..2].iter().any(|w| notice.contains(w));
    Some(Refusal {
        at: record_timestamp(line)?,
        model,
        logged_out: false,
        window: if five_hour { Window::FiveHour } else { Window::Week },
    })
}

/// The text of a queued notice that a background agent stopped.
fn agent_notice(line: &str) -> Option<&str> {
    if record_type(line)? != "queue-operation" || top_level_str(line, "operation")? != "enqueue" {
        return None;
    }
    top_level_str(line, "content")
        .filter(|c| c.starts_with("<task-notification>") && c.contains("<summary>Agent "))
}

/// The CLI's error code for an account it could not log in to.
fn says_logged_out(line: &str) -> bool {
    line.contains("\"error\":\"authentication_failed\"")
}

/// The model a refusal names, or empty when it cannot be read.
///
/// The CLI's two templates put it in fixed places: "You've reached
/// your Fable limit." and "Opus 5.5 requires usage credits.".
fn refused_model(line: &str) -> String {
    const LIMIT: &str = "You've reached your ";
    if let Some(i) = line.find(LIMIT) {
        let rest = &line[i + LIMIT.len()..];
        return rest.find(" limit").map(|j| short_model(&rest[..j])).unwrap_or_default();
    }
    match line.find(" requires usage credits") {
        // the text field opens with the model's display name
        Some(j) => short_model(&line[line[..j].rfind('"').map_or(0, |q| q + 1)..j]),
        None => String::new(),
    }
}

/// A refusal, if it reached the claude that started at `started`.
pub(super) fn refused_since(refusal: Option<Refusal>, started: SystemTime) -> Option<Refusal> {
    let start = started.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    refusal.filter(|r| r.at >= start)
}

/// The ways the CLI says "this account cannot serve you".
fn says_out_of_quota(line: &str) -> bool {
    (line.contains("You've reached your") && line.contains("limit"))
        || line.contains("requires usage credits")
        || ACCOUNT_WINDOWS.iter().any(|w| line.contains(w))
}

/// The account-wide windows, as the CLI words running out of them.
///
/// Not "You've hit your" alone: the same opening is used for a fast
/// mode limit and a monthly spend limit, neither of which shuts the
/// account out.
/// The first two are the five-hour window under its two names.
const ACCOUNT_WINDOWS: [&str; 3] = [
    "You've hit your session limit",
    "You've hit your 5-hour limit",
    "You've hit your weekly limit",
];

/// The `timestamp` field of one transcript record.
fn record_timestamp(line: &str) -> Option<i64> {
    let key = "\"timestamp\":\"";
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find('"')?;
    marspot::cc_usage::parse_iso_utc(rest[..end].to_string())
}

/// Treat as logged out every profile a pane has found it cannot log in
/// to since the feed last read it.
///
/// `seen` is, per profile, the newest login failure any pane met
/// there. The feed keeps reporting such an account by its old numbers
/// -- it cannot tell it from an idle one -- so without this every
/// refused pane would be sent back to it. A newer reading means
/// someone has logged in again, and the feed is believed once more.
pub(super) fn mark_logged_out(rooms: &mut [(u8, Room)], seen: &HashMap<u8, i64>) {
    for (p, room) in rooms.iter_mut() {
        if let Room::Known { status, measured_at, .. } = room
            && seen.get(p).is_some_and(|at| at > measured_at)
        {
            *status = "auth_error".into();
        }
    }
}

/// A pane was told by the API that it had run out.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) struct Refusal {
    /// Unix seconds, from the record's own timestamp.
    pub at: i64,
    /// The model the refusal names, as `short_model` spells it, or
    /// empty when it could not be read.
    ///
    /// Every refusal the CLI writes names one: "You've reached your
    /// Fable limit." or "Opus 5.5 requires usage credits.". It says
    /// nothing about any other model on the same account: on
    /// 2026-10-03 a Fable refusal moved a pane that had just been
    /// switched to Opus, onto an account where it ran out again.
    pub model: String,
    /// The account could not log in at all, rather than being out of
    /// quota.
    ///
    /// The feed cannot tell a logged-out account from an idle one:
    /// both show as an access token that has expired and was never
    /// refreshed. Only a claude that tried to use it finds out, and
    /// that is the only place this is learnt. It names no model.
    pub logged_out: bool,
    /// Which of the account's windows ran out.
    pub window: Window,
}

/// The window a refusal is about, which is how long it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::plugins) enum Window {
    /// "session" and "5-hour": over within five hours.
    FiveHour,
    /// The week, a model's cap, or credits.
    Week,
}

impl Refusal {
    /// Does this refusal shut out a pane working in `model`?
    ///
    /// One that names no model covers every pane. One that names a
    /// model covers only panes on it: with no model given, nothing is
    /// assumed, Fable included.
    pub(super) fn applies_to(&self, model: Option<&str>) -> bool {
        if self.model.is_empty() {
            return true;
        }
        model.is_some_and(|m| same_model(&m.to_ascii_lowercase(), &self.model))
    }
}
