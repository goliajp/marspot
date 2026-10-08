//! What a pane learns for itself about its account, from the records
//! the CLI writes into its transcript: that it ran out, or that it
//! could not log in.

use std::collections::HashMap;
use std::time::SystemTime;

use super::model::short_model;
use super::quota::{Refusal, Room};

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
/// Two texts are matched, both read out of the CLI's own templates
/// rather than guessed: the model-limit branch and the credits branch.
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
    Some(Refusal {
        at: record_timestamp(line)?,
        model: if logged_out { String::new() } else { refused_model(line) },
        logged_out,
    })
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

/// The two ways the CLI says "this account cannot serve you".
fn says_out_of_quota(line: &str) -> bool {
    (line.contains("You've reached your") && line.contains("limit"))
        || line.contains("requires usage credits")
}

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
