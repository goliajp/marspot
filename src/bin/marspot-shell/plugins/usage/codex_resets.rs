//! A Codex account's stored resets, from the two places they can be
//! read without asking anything new: the `rateLimitResetCredits` part
//! of the `account/rateLimits/read` answer the collector already gets,
//! and the devops feed, as its `resets` block or, before devops writes
//! one, the `resets_available` and `resets_expire_at` of its credits.
//!
//! Each credit is one grant.  Where the answer gives no end for a
//! credit the feed's end is taken, which is the earliest one: every
//! credit without an end of its own is said to end then.

use marspot::agent_resets::{AccountResets, Grant, Resets};
use marspot_agent::types::reset::{Agreement, Count, agree};
use marspot_peer::json::{self, Value};

use super::codex::Replies;

/// What one credit brings back, by plan: the main allowance, which on
/// Plus is the five-hour window and the week, on Pro the week.
fn restores(plan: &str) -> Vec<String> {
    let windows: &[&str] = if plan.eq_ignore_ascii_case("plus") { &["five_hour", "seven_day"] } else { &["seven_day"] };
    windows.iter().map(|w| w.to_string()).collect()
}

fn credit_grant(plan: &str, left: u32, starts_at: Option<i64>, ends_at: Option<i64>, unavailable: Option<&str>) -> Grant {
    Grant {
        kind: "codex_full".into(),
        restores: restores(plan),
        moves_weekly_reset: true,
        left: Some(left),
        total: Some(1),
        usable_now: unavailable.is_some().then_some(false),
        use_requires_limit: Some(false),
        unavailable_reason: unavailable.map(str::to_string),
        starts_at,
        ends_at,
        ..Grant::default()
    }
}

/// The resets in one app-server's answers, or `None` when the answer
/// to the limits request carries no credit list.
pub(crate) fn of_replies(replies: &Replies, now: i64) -> Option<Resets> {
    let result = replies.limits.as_ref()?.get("result")?;
    let credits = result.get("rateLimitResetCredits").filter(|c| **c != Value::Null)?;
    let plan = replies
        .account
        .as_ref()
        .and_then(|a| a.at(&["result", "account", "planType"]))
        .or(result.at(&["rateLimits", "planType"]))
        .and_then(Value::str)
        .unwrap_or_default();
    let grants = credits
        .get("credits")
        .map(Value::arr)
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let (left, unavailable) = match c.str_at("status")? {
                "available" => (1, None),
                "redeeming" => (0, Some("redeeming")),
                _ => return None,
            };
            Some(credit_grant(plan, left, c.get("grantedAt").and_then(Value::i64), c.get("expiresAt").and_then(Value::i64), unavailable))
        })
        .collect();
    let mut r = Resets { grants, ..Resets::none(now, "app_server") };
    r.eligible = Some(true);
    // a count without the list behind it: unknown credits, no end
    let count = credits.get("availableCount").and_then(Value::i64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
    for _ in r.left_total()..count {
        r.grants.push(credit_grant(plan, 1, None, None, None));
    }
    Some(r)
}

/// Each account of a devops Codex feed with what it says of resets.
pub(super) fn of_feed(body: &str) -> Vec<(String, Resets)> {
    let Ok(v) = json::parse(body) else { return Vec::new() };
    v.get("accounts").map(Value::arr).unwrap_or_default().iter().filter_map(account_of_feed).collect()
}

/// Whether a devops Codex feed gives some account's credits only as a
/// count and one end, not one by one.  The collector then reads the
/// credits itself, each with its own start and end.
pub(super) fn feed_lacks_grants(body: &str) -> bool {
    let Ok(v) = json::parse(body) else { return false };
    v.get("accounts").map(Value::arr).unwrap_or_default().iter().any(|row| {
        let listed = row.get("resets").and_then(Resets::from_value).is_some();
        let counted = row.at(&["credits", "resets_available"]).and_then(Value::i64).is_some_and(|n| n > 0);
        counted && !listed
    })
}

fn account_of_feed(row: &Value) -> Option<(String, Resets)> {
    let name = row.str_at("name")?.to_string();
    if let Some(r) = row.get("resets").and_then(Resets::from_value) {
        return Some((name, r));
    }
    let credits = row.get("credits")?;
    let left = u32::try_from(credits.get("resets_available")?.i64()?).ok()?;
    let ends = credits.get("resets_expire_at").and_then(Value::i64);
    let observed = marspot::cc_usage::parse_iso_utc(row.str_at("collected_at")?.to_string())?;
    let plan = row.str_at("tier").unwrap_or_default();
    let grants = (0..left).map(|_| credit_grant(plan, 1, None, ends, None)).collect();
    Some((name, Resets { grants, ..Resets::none(observed, "devops_feed") }))
}

fn count(r: &Resets) -> Count {
    Count { observed_at: r.observed_at, left: r.left_total(), next_expiry: r.next_expiry() }
}

/// Lay the feed's readings over what the collector read itself: a
/// credit with no end of its own takes the feed's earliest end while
/// that is still ahead; and say for each account read both ways
/// whether the two agree.
pub(super) fn settle(own: &mut [AccountResets], feed: &[(String, Resets)], now: i64) -> Vec<(String, Agreement, Count, Count)> {
    let mut said = Vec::new();
    for a in own.iter_mut() {
        let Some((_, theirs)) = feed.iter().find(|(name, _)| *name == a.name) else { continue };
        if let Some(end) = theirs.next_expiry().filter(|e| *e > now) {
            for g in a.resets.grants.iter_mut().filter(|g| g.ends_at.is_none()) {
                g.ends_at = Some(end);
            }
        }
        let (mine, other) = (count(&a.resets), count(theirs));
        said.push((a.name.clone(), agree(mine, other), mine, other));
    }
    said
}

#[cfg(test)]
#[path = "codex_resets_tests.rs"]
mod tests;
