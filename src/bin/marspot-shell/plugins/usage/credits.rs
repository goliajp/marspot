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

use marspot_peer::json::{self, obj, Value};
use marspot_term::{lx_info, lx_warn};

use super::feed::Account;
use super::samples::{profile_of, Balance, Samples};
use super::feed::Credits;
use marspot_agent::types::wallet::{Cap, Lot, View};

/// The window the spend rate is taken over; `View::spent_1h` is summed
/// over it.
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
/// their rollouts show; the newer of the two readings gives the balance
/// and the cap.  The account's lot, once one is recorded, is its only
/// expiry, inferred, with whose sighting dated it; before that both are
/// unknown.
pub(super) fn enrich(accounts: &mut [Account], views: &[(u8, View)], now: i64) {
    for a in accounts {
        let Some(c) = a.credits.as_mut() else { continue };
        let Some((_, v)) = profile_of(&a.name).and_then(|p| views.iter().find(|(q, _)| *q == p)) else { continue };
        if c.observed_at.is_none_or(|at| v.at > at) {
            (c.balance, c.observed_at, c.cap) = (Some(v.balance), Some(v.at), v.cap.clone());
        }
        c.spending = Some(v.spending(now));
        c.spent_24h = Some(v.spent_24h);
        c.rate_per_h = Some(v.spent_1h);
        c.drops = Some(v.drops.clone());
        c.lots = v.lot.clone().map(|l| vec![l]);
        c.expiry_source = v.lot.as_ref().map(|_| "inferred".to_string());
    }
}

/// The spend fields of a feed's `credits` object, set on `v`; `null`
/// for each unknown.
pub(super) fn wallet_fields(v: &mut Value, c: &Credits) {
    let num = |n: Option<f64>| n.map_or(Value::Null, Value::Num);
    let text = |s: &Option<String>| s.as_deref().map_or(Value::Null, Value::from);
    let flag = |b: Option<bool>| b.map_or(Value::Null, Value::from);
    let lot = |l: &Lot| obj([("amount", Value::Num(l.amount)), ("expires_at", l.expires_at.into()), ("bound", l.bound.as_str().into()), ("seen_by", text(&l.seen_by))]);
    let cap = |c: &Cap| {
        obj([("reached", flag(c.reached)), ("limit", num(c.limit)), ("remaining_percent", num(c.remaining_percent)), ("reached_type", text(&c.reached_type))])
    };
    let drop = |(at, amount): &(i64, f64)| obj([("at", (*at).into()), ("amount", Value::Num(*amount))]);
    v.set("unit", text(&c.unit));
    v.set("observed_at", c.observed_at.map_or(Value::Null, Value::from));
    v.set("spend", text(&c.spend));
    v.set("spending", flag(c.spending));
    v.set("spent_24h", num(c.spent_24h));
    v.set("rate_per_h", num(c.rate_per_h));
    v.set("lots", c.lots.as_ref().map_or(Value::Null, |l| Value::Arr(l.iter().map(lot).collect())));
    v.set("expiry_source", text(&c.expiry_source));
    v.set("cap", c.cap.as_ref().map_or(Value::Null, cap));
    v.set("unexplained_drops", c.drops.as_ref().map_or(Value::Null, |d| Value::Arr(d.iter().map(drop).collect())));
}

/// The spend fields of a feed's `credits` object as `wallet_fields`
/// wrote them, into `c`.
pub(super) fn read_wallet_fields(v: &Value, c: &mut Credits) {
    let num = |v: &Value, k: &str| match v.get(k) {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    };
    let text = |k: &str| v.str_at(k).map(str::to_string);
    let present = |k: &str| v.get(k).filter(|x| **x != Value::Null);
    (c.unit, c.spend, c.expiry_source) = (text("unit"), text("spend"), text("expiry_source"));
    c.observed_at = v.get("observed_at").and_then(Value::i64);
    c.spending = v.get("spending").and_then(Value::bool);
    (c.spent_24h, c.rate_per_h) = (num(v, "spent_24h"), num(v, "rate_per_h"));
    c.lots = present("lots").map(|l| {
        let lot = |x: &Value| {
            let seen_by = x.str_at("seen_by").map(str::to_string);
            Some(Lot { amount: num(x, "amount")?, expires_at: x.get("expires_at")?.i64()?, bound: x.str_at("bound")?.to_string(), seen_by })
        };
        l.arr().iter().filter_map(lot).collect()
    });
    c.cap = present("cap").map(|x| Cap {
        reached: x.get("reached").and_then(Value::bool),
        limit: num(x, "limit"),
        remaining_percent: num(x, "remaining_percent"),
        reached_type: x.str_at("reached_type").map(str::to_string),
    });
    c.drops = present("unexplained_drops").map(|d| d.arr().iter().filter_map(|x| Some((x.get("at")?.i64()?, num(x, "amount")?))).collect());
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
            let rate = views.iter().find(|(p, _)| *p == mine.profile).map_or(0.0, |(_, v)| v.spent_1h / RATE_OVER_S as f64);
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
