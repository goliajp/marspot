//! The Codex accounts' paid balances, each pass: what marspot read
//! itself (the app-server's answer or the rollouts' newest reading,
//! whichever is newer) goes to the samples, fills the spend fields of
//! marspot's own feed, and is checked against the devops feed's reading
//! while that feed is fresh.  Two readings taken at different moments
//! agree when they differ by no more than the account spends between the
//! two moments at its last hour's rate, plus one credit; marspot's own
//! reading is the one believed either way.  A verdict is logged when it
//! changes.  A stale or missing devops feed is not compared with.

use std::collections::HashMap;

use marspot_peer::json::{self, Value};
use marspot_term::{lx_info, lx_warn};

use super::feed::Account;
use super::samples::{profile_of, Balance, Samples};
use crate::agentsvc::accounts::wallet::View;

/// The window the spend rate is taken over.
const RATE_OVER_S: i64 = 3600;

/// marspot's own reading of each Codex account's balance: the newer of
/// the app-server's and the rollouts'.  An account whose balance read
/// neither way has none.
pub(super) fn own_balances(accounts: &[Account], views: &[(u8, View)]) -> Vec<Balance> {
    let mut out: Vec<Balance> = accounts
        .iter()
        .filter_map(|a| {
            let c = a.credits.as_ref()?;
            let at = c.observed_at.unwrap_or(a.credits_seen);
            Some(Balance { name: a.name.clone(), profile: profile_of(&a.name)?, at, balance: c.balance?, unit: "credits" })
        })
        .collect();
    for (profile, v) in views {
        match out.iter_mut().find(|b| b.profile == *profile) {
            Some(b) if b.at >= v.at => {}
            Some(b) => (b.at, b.balance) = (v.at, v.balance),
            None => out.push(Balance { name: format!("Codex {profile}"), profile: *profile, at: v.at, balance: v.balance, unit: "credits" }),
        }
    }
    out.sort_by_key(|b| b.profile);
    out
}

/// Fill the spend fields of the Codex accounts marspot writes from what
/// their rollouts show; the newer balance of the two readings stands.
pub(super) fn enrich(accounts: &mut [Account], views: &[(u8, View)]) {
    for a in accounts {
        let Some(c) = a.credits.as_mut() else { continue };
        let Some((_, v)) = profile_of(&a.name).and_then(|p| views.iter().find(|(q, _)| *q == p)) else { continue };
        if c.observed_at.is_none_or(|at| v.at > at) {
            (c.balance, c.observed_at) = (Some(v.balance), Some(v.at));
        }
        c.spending = Some(v.spending());
        c.spent_24h = Some(v.spent_since(v.at - 86_400));
        c.rate_per_h = Some(v.spent_since(v.at - RATE_OVER_S));
        c.drops = Some(v.drops.clone());
        if v.cap.is_some() {
            c.cap = v.cap.clone();
        }
    }
}

/// The balances of one pass: sampled, and checked against the devops
/// feed `feed` when it is fresh.
pub(super) fn pass(samples: &mut Samples, checks: &mut Checks, codex: &[Account], feed: Option<&str>, views: &[(u8, View)], now: i64) {
    let own = own_balances(codex, views);
    match samples.record_wallet("codex", &own, now) {
        Ok(0) => {}
        Ok(rows) => lx_info!("usage.samples.written", "own balances sampled", provider = "codex", rows = rows),
        Err(e) => lx_warn!("usage.samples.write_failed", &e.to_string(), provider = "codex"),
    }
    checks.check(feed, &own, views);
}

/// One account read both ways.
#[derive(Debug, PartialEq)]
pub(super) struct Verdict {
    pub name: String,
    pub agree: bool,
    pub line: String,
}

/// Each account the devops feed `body` and marspot both have a balance
/// for, compared.
pub(super) fn compare(body: &str, own: &[Balance], views: &[(u8, View)]) -> Vec<Verdict> {
    let Ok(v) = json::parse(body) else { return Vec::new() };
    let generated = v.str_at("generated_at").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string()));
    let rows = v.get("accounts").map(Value::arr).unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            let name = row.str_at("name")?;
            let theirs = match row.at(&["credits", "balance"])? {
                Value::Num(n) => *n,
                _ => return None,
            };
            let their_at = row.str_at("collected_at").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string())).or(generated)?;
            let mine = own.iter().find(|b| b.name == name)?;
            let rate = views.iter().find(|(p, _)| *p == mine.profile).map_or(0.0, |(_, v)| v.spent_since(v.at - RATE_OVER_S) / RATE_OVER_S as f64);
            let tolerance = rate * (mine.at - their_at).abs() as f64 + 1.0;
            let agree = (mine.balance - theirs).abs() <= tolerance;
            let line = format!(
                "account={name} own={:.6} own_at={} feed={theirs:.6} feed_at={their_at} tolerance={tolerance:.3}",
                mine.balance, mine.at
            );
            Some(Verdict { name: name.to_string(), agree, line })
        })
        .collect()
}

/// The last verdict logged per account.
#[derive(Default)]
pub(super) struct Checks {
    told: HashMap<String, bool>,
}

impl Checks {
    /// Compare with the devops feed when it is fresh (`feed` is `None`
    /// otherwise) and log each verdict that changed; the verdicts.
    pub(super) fn check(&mut self, feed: Option<&str>, own: &[Balance], views: &[(u8, View)]) -> Vec<Verdict> {
        let Some(body) = feed else { return Vec::new() };
        let verdicts = compare(body, own, views);
        for v in &verdicts {
            if self.told.insert(v.name.clone(), v.agree) == Some(v.agree) {
                continue;
            }
            if v.agree {
                lx_info!("usage.credits.agree", &v.line);
            } else {
                lx_warn!("usage.credits.disagree", &v.line);
            }
        }
        verdicts
    }
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod tests;
