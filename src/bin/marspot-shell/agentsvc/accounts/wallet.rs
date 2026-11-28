//! Each agent account's paid balance as the agent's own records last
//! read it: the newest reading, what was spent in the last day, and the
//! drops nothing explains.  The adapter that reads the records publishes
//! it after each pass; the usage collector reads it for its feed, its
//! samples and its check against another collector's reading.
//!
//! Only records: nothing here decides where a pane runs.

use std::sync::Mutex;

use marspot_peer::json::Value;

/// One row per agent and account at most.
const MAX_ROWS: usize = 512;

/// A spend this recent, with the plan allowance still used up, is an
/// account spending now.
pub(crate) const SPENDING_WITHIN_S: i64 = 900;

/// A spending cap the provider reports.  Every field is what the
/// provider said; `None` where it said nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Cap {
    pub reached: Option<bool>,
    pub limit: Option<f64>,
    pub remaining_percent: Option<f64>,
    /// The refusal kind when it is about credits.
    pub reached_type: Option<String>,
}

impl Cap {
    /// The cap in a rate-limits object; `keys` names its individual
    /// limit, spend-control and refusal-kind fields, and the limit's
    /// two fields, in the spelling of the record read.  `None` when the
    /// object says nothing about one.
    pub(crate) fn of(rate: &Value, keys: [&str; 5]) -> Option<Cap> {
        let [individual, control, reached, limit, remaining] = keys;
        let num = |v: Option<&Value>| match v {
            Some(Value::Num(n)) => Some(*n),
            Some(Value::Str(s)) => s.parse().ok(),
            _ => None,
        };
        let ind = rate.get(individual).filter(|v| **v != Value::Null);
        let cap = Cap {
            reached: rate.get(control).and_then(Value::bool),
            limit: ind.and_then(|i| num(i.get(limit))),
            remaining_percent: ind.and_then(|i| num(i.get(remaining))),
            reached_type: rate.str_at(reached).filter(|t| t.contains("credits")).map(str::to_string),
        };
        (cap != Cap::default()).then_some(cap)
    }
}

/// A batch of credits and when it ends at the latest: the balance an
/// account was first seen with, ending `bound` = `latest` (the grant
/// was no later than that sighting, so its true end may be sooner).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Lot {
    pub amount: f64,
    pub expires_at: i64,
    pub bound: String,
}

/// What one account's records say of its balance.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct View {
    /// When the newest reading was made, unix seconds.
    pub at: i64,
    pub balance: f64,
    /// The plan allowance was used up at that reading.
    pub week_full: bool,
    pub cap: Option<Cap>,
    /// What was spent in the day and in the hour before the pass that
    /// published this, and when the last spend was.
    pub spent_24h: f64,
    pub spent_1h: f64,
    pub last_spend: Option<i64>,
    /// `(at, amount)` of the recent drops with the allowance not used up.
    pub drops: Vec<(i64, f64)>,
    pub lot: Option<Lot>,
}

impl View {
    pub(crate) fn spending(&self, now: i64) -> bool {
        self.week_full && self.last_spend.is_some_and(|at| at >= now - SPENDING_WITHIN_S)
    }
}

static TABLE: Mutex<Vec<(&'static str, u8, View)>> = Mutex::new(Vec::new());

/// Replace `agent`'s rows with what its last pass knew.
pub(crate) fn publish(agent: &'static str, rows: impl IntoIterator<Item = (u8, View)>) {
    let mut t = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    t.retain(|(a, _, _)| *a != agent);
    for (profile, view) in rows {
        if t.len() >= MAX_ROWS {
            break;
        }
        t.push((agent, profile, view));
    }
}

/// Every account of `agent` with a reading, by account number.
pub(crate) fn all(agent: &str) -> Vec<(u8, View)> {
    let t = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    t.iter().filter(|(a, _, _)| *a == agent).map(|(_, p, v)| (*p, v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cap_is_read_in_either_spelling_and_nothing_said_is_none() {
        let app = marspot_peer::json::parse(
            r#"{"individualLimit":{"limit":"500.5","remainingPercent":20},"spendControlReached":true,"rateLimitReachedType":"workspace_member_credits_depleted"}"#,
        )
        .unwrap();
        let cap = Cap::of(&app, ["individualLimit", "spendControlReached", "rateLimitReachedType", "limit", "remainingPercent"]);
        assert_eq!(
            cap,
            Some(Cap { reached: Some(true), limit: Some(500.5), remaining_percent: Some(20.0), reached_type: Some("workspace_member_credits_depleted".into()) })
        );
        let rollout = marspot_peer::json::parse(r#"{"individual_limit":null,"spend_control_reached":null,"rate_limit_reached_type":"rate_limit_reached"}"#).unwrap();
        let keys = ["individual_limit", "spend_control_reached", "rate_limit_reached_type", "limit", "remaining_percent"];
        assert_eq!(Cap::of(&rollout, keys), None, "a plain limit is no credits cap");
    }

    #[test]
    fn spending_needs_the_allowance_used_up_and_a_recent_spend() {
        let mut v = View { at: 9_000, balance: 5.0, week_full: true, last_spend: Some(9_500), ..View::default() };
        assert!(v.spending(10_000), "judged at the time asked, not at the reading's");
        assert!(!v.spending(9_500 + SPENDING_WITHIN_S + 1), "nothing spent for a while");
        v.week_full = false;
        assert!(!v.spending(10_000));
    }

    #[test]
    fn a_pass_replaces_its_agents_rows() {
        const AGENT: &str = "test-wallet";
        publish(AGENT, [(1, View { balance: 3.0, ..View::default() }), (2, View::default())]);
        assert_eq!(all(AGENT).len(), 2);
        publish(AGENT, [(2, View { balance: 1.0, ..View::default() })]);
        assert_eq!(all(AGENT), vec![(2, View { balance: 1.0, ..View::default() })]);
    }
}
