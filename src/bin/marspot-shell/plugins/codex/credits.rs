//! What each codex account's paid credits are doing: the balance its
//! panes' rollouts and the usage feed report, and whether it fell while
//! the account's week was used up.
//!
//! Codex spends credits by itself once a week is used up, without a
//! refusal and without saying so; a balance that falls in that state is
//! the only sign of it.

use std::collections::HashMap;

use marspot_peer::json;

use super::alloc::{AccountIn, Snapshot, WEEK_S};
use super::alloc_rollout::balance_of;

/// A week counts as used up from this fraction (the answers give whole
/// percents, so 100% reads as 1.0).
pub(super) const SPENT: f64 = 0.999;

/// A drop smaller than this is a reading's rounding, not spending.
const FALL_EPS: f64 = 0.001;

/// Renewals this close are the same week (two reads can differ by a second).
const SAME_WEEK_S: i64 = 86_400;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Wallet {
    last: Option<(i64, f64)>,
    /// The renewal of the week in which the balance last fell while
    /// that week was used up.
    fell_in: Option<i64>,
}

/// Per account, its newest balance and whether it fell; keyed by
/// profile number, so bounded by it.
#[derive(Debug, Default)]
pub(super) struct Wallets(HashMap<u8, Wallet>);

impl Wallets {
    /// A balance read at `at`; `spent` is the renewal of the account's
    /// week when that week was used up at the reading.  A reading no
    /// newer than the last is an old one.
    pub(super) fn saw(&mut self, profile: u8, at: i64, balance: f64, spent: Option<i64>) {
        let w = self.0.entry(profile).or_default();
        if w.last.is_some_and(|(was, _)| at <= was) {
            return;
        }
        if let (Some((_, before)), Some(renews)) = (w.last, spent)
            && balance < before - FALL_EPS
        {
            w.fell_in = Some(renews);
        }
        w.last = Some((at, balance));
    }

    pub(super) fn balance(&self, profile: u8) -> Option<(i64, f64)> {
        self.0.get(&profile)?.last
    }

    /// Its balance fell during the week that renews at `renews`.
    pub(super) fn fell(&self, profile: u8, renews: i64) -> bool {
        self.0.get(&profile).and_then(|w| w.fell_in).is_some_and(|r| (r - renews).abs() < SAME_WEEK_S)
    }
}

/// One codex account as the credits rule sees it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Acct {
    pub profile: u8,
    /// The newer of the feed's and a rollout's week reading.
    pub week_used: f64,
    pub renews: i64,
    /// When that reading was taken.
    pub at: i64,
    pub open: bool,
    pub balance: Option<(i64, f64)>,
    /// Its balance fell during this week while the week was used up.
    pub falling: bool,
}

impl Acct {
    /// Its week is used up (or the feed says it refuses) and not renewed.
    pub(super) fn exhausted(&self, now: i64) -> bool {
        self.renews > now && (self.week_used >= SPENT || !self.open)
    }

    /// It runs its panes on paid credits.
    pub(super) fn paying(&self, now: i64) -> bool {
        self.exhausted(now) && self.falling
    }
}

/// The renewal of `a`'s week when the feed reads it as used up at `now`.
pub(super) fn spent_week(a: &AccountIn, now: i64) -> Option<i64> {
    (a.week_renews > now && (a.week_used >= SPENT || !a.open)).then_some(a.week_renews)
}

/// Every account of the snapshot, judged by the newer of the feed's and
/// a rollout's week reading.
pub(super) fn accounts(snap: &Snapshot, limits: &HashMap<u8, (i64, f64, i64)>, wallets: &Wallets) -> Vec<Acct> {
    snap.accounts
        .iter()
        .map(|a| {
            let (mut week_used, mut renews, at) = match limits.get(&a.profile) {
                Some((at, used, renews)) if *at > a.measured_at => (*used, *renews, *at),
                _ => (a.week_used, a.week_renews, a.measured_at),
            };
            if renews <= snap.now {
                (week_used, renews) = (0.0, renews + WEEK_S);
            }
            let falling = wallets.fell(a.profile, renews);
            Acct { profile: a.profile, week_used, renews, at, open: a.open, balance: wallets.balance(a.profile), falling }
        })
        .collect()
}

/// When the codex usage feed was written and each account's credits
/// balance in it, by profile number (`Codex N` is profile N).  The feed
/// is another program's file.
pub(super) fn feed_balances(body: &str) -> Option<(i64, Vec<(u8, f64)>)> {
    let v = json::parse(body).ok()?;
    let at = v.str_at("generated_at").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string()))?;
    let list = v
        .get("accounts")?
        .arr()
        .iter()
        .filter_map(|a| {
            let name = a.str_at("name")?;
            let n = if name == "Codex" { 0 } else { name.strip_prefix("Codex ")?.parse().ok()? };
            Some((n, balance_of(a)?))
        })
        .collect();
    Some((at, list))
}
