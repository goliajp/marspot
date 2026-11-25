//! What the kimi feed says beyond the five hours and the week that
//! today's choices read: the monthly allowance, and the wallet an
//! account goes on to pay from once its allowance is used.
//!
//! Read only while `kimi.allocation` is not `off`.  An account whose
//! month is spent takes nothing until the month renews; one that has
//! used up an allowance and still has money in its wallet would go on
//! by paying, and is treated as spent too, so no pane is sent to spend
//! money.  Both are judged from the feed alone: kimi's own reply is the
//! only source, and no real account's reply has been seen yet.
//!
//! While a window reads spent, today's choices already refuse the
//! account, so paying changes a decision only after that: an account
//! seen paying stays shut until the five hours it was seen in renew,
//! even when a later reading of those five hours is below full, as long
//! as its wallet still holds money (see `Paying`).

use std::collections::HashMap;

use marspot_peer::json::{self, Value};

use crate::plugins::claudecode::quota::Room;

/// The most account rows read from one feed.
const MAX_ACCOUNTS: usize = 64;
const FIVE_HOURS_S: i64 = 5 * 3600;

/// One window as the feed wrote it: used, and when it renews.
pub(super) type Reading = (f64, Option<i64>);

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Extra {
    pub five_hour: Option<Reading>,
    pub week: Option<Reading>,
    pub month: Option<Reading>,
    /// What is left in the paid wallet.
    pub wallet_left: Option<f64>,
    /// The feed's status for the row is `rejected`.
    pub rejected: bool,
    /// Until when it counts as paying from an earlier reading.
    pub paying_until: Option<i64>,
}

/// Why an account takes no pane, beyond what today's choices know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shut {
    MonthSpent,
    Paying,
}

impl Shut {
    pub(super) fn word(self) -> &'static str {
        match self {
            Shut::MonthSpent => "month_spent",
            Shut::Paying => "paying",
        }
    }
}

fn spent(w: Option<Reading>, now: i64) -> bool {
    w.is_some_and(|(used, renews)| used >= 1.0 && renews.is_none_or(|r| r > now))
}

impl Extra {
    pub(super) fn shut(&self, now: i64) -> Option<Shut> {
        if spent(self.month, now) {
            return Some(Shut::MonthSpent);
        }
        let earlier = self.paying_until.is_some_and(|until| until > now);
        (self.pays(now) || (earlier && self.can_pay())).then_some(Shut::Paying)
    }

    fn can_pay(&self) -> bool {
        self.wallet_left.is_some_and(|left| left > 0.0)
    }

    /// Its allowance is used and its wallet would pay for what follows.
    fn pays(&self, now: i64) -> bool {
        let used = self.rejected || spent(self.five_hour, now) || spent(self.week, now) || spent(self.month, now);
        used && self.can_pay()
    }
}

/// The accounts seen paying, each until the end of the five hours it
/// was seen in; at most one entry per profile number.
#[derive(Debug, Default)]
pub(super) struct Paying(HashMap<u8, i64>);

impl Paying {
    /// Take in a fresh read of the feed, and mark each account still
    /// within the five hours it was seen paying in.
    pub(super) fn carry(&mut self, extras: &mut Extras, now: i64) {
        self.0.retain(|_, until| *until > now);
        for (n, e) in extras.iter_mut() {
            if e.pays(now) {
                // without a renewal the five hours are counted from now
                let until = e.five_hour.and_then(|(_, r)| r).filter(|r| *r > now).unwrap_or(now + FIVE_HOURS_S);
                let held = self.0.entry(*n).or_insert(until);
                *held = (*held).max(until);
            }
            e.paying_until = self.0.get(n).copied();
        }
    }
}

pub(super) type Extras = HashMap<u8, Extra>;

/// The profile a feed row names: `Kimi` is the default home, `Kimi N`
/// profile N.
fn profile_of(name: &str) -> Option<u8> {
    match name {
        "Kimi" => Some(0),
        _ => name.strip_prefix("Kimi ")?.parse().ok(),
    }
}

/// The rows of a kimi feed's text.
pub(super) fn parse(body: &str) -> Extras {
    let Ok(v) = json::parse(body) else { return Extras::new() };
    let num = |row: &Value, key: &str| match row.get(key) {
        Some(Value::Num(n)) if n.is_finite() => Some(*n),
        _ => None,
    };
    let reading = |row: &Value, util: &str, reset: &str| num(row, util).map(|u| (u, row.get(reset).and_then(Value::i64)));
    v.get("accounts")
        .map(Value::arr)
        .unwrap_or_default()
        .iter()
        .take(MAX_ACCOUNTS)
        .filter_map(|row| {
            let n = profile_of(row.str_at("name")?)?;
            let wallet_left = row.get("credits").and_then(|c| num(c, "balance"));
            let extra = Extra {
                five_hour: reading(row, "utilization_5h", "reset_5h"),
                week: reading(row, "utilization_7d", "reset_7d"),
                month: reading(row, "utilization_month", "reset_month"),
                wallet_left,
                rejected: row.str_at("status") == Some("rejected"),
                paying_until: None,
            };
            Some((n, extra))
        })
        .collect()
}

/// The kimi feed as the collector last wrote it.  Reads a file.
pub(super) fn read() -> Extras {
    std::fs::read_to_string(marspot::kimi_usage::own_feed_path()).map(|t| parse(&t)).unwrap_or_default()
}

/// Shut each account of `rooms` that `extras` says takes nothing, the
/// way a refusal shuts one: its five hours read as spent.  Returns the
/// accounts shut, with why.
pub(super) fn mark(rooms: &mut [(u8, Room)], extras: &Extras, now: i64) -> Vec<(u8, Shut)> {
    let mut shut = Vec::new();
    for (n, room) in rooms.iter_mut() {
        let Some(why) = extras.get(n).and_then(|e| e.shut(now)) else { continue };
        if let Room::Known { util_5h, .. } = room {
            *util_5h = Some(1.0);
            shut.push((*n, why));
        }
    }
    shut
}
