//! Reading back a feed file this module wrote.

use marspot_peer::json::{self, Value};

use super::feed::{Account, Credits, ModelWindow, Window, WEEK_MINUTES};
use crate::agentsvc::accounts::wallet::Cap;

/// The accounts in a feed file this module wrote, so a restart carries
/// on from the last observations instead of from nothing.
pub(super) fn load(body: &str) -> Vec<Account> {
    let Ok(v) = json::parse(body) else {
        return Vec::new();
    };
    v.get("accounts").map(Value::arr).unwrap_or_default().iter().map(account_of).collect()
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

fn iso_at(v: &Value, key: &str) -> Option<i64> {
    marspot::cc_usage::parse_iso_utc(v.str_at(key)?.to_string())
}

fn account_of(row: &Value) -> Account {
    let collected = iso_at(row, "collected_at").unwrap_or(0);
    let limits_seen = iso_at(row, "limits_as_of").unwrap_or(collected);
    let own = |util: &str, reset: &str| {
        let reset = row.get(reset).and_then(Value::i64);
        f64_at(row, util).map(|u| Window::new(u, reset, collected))
    };
    let models = row.get("model_limits").map(Value::arr).unwrap_or_default();
    let credits = row.get("credits").filter(|c| **c != Value::Null).map(credits_of);
    let mut a = Account {
        name: row.str_at("name").unwrap_or_default().to_string(),
        email: row.str_at("email").unwrap_or_default().to_string(),
        tier: row.str_at("tier").unwrap_or_default().to_string(),
        auth_error: row.str_at("status") == Some("auth_error"),
        limit_reached: false,
        five_hour: own("utilization_5h", "reset_5h"),
        seven_day: own("utilization_7d", "reset_7d"),
        month: own("utilization_month", "reset_month"),
        models: models.iter().filter_map(|m| model_of(m, limits_seen)).collect(),
        credits_seen: if credits.is_some() { collected } else { 0 },
        credits,
        last_read: collected,
    };
    // a refusal the windows do not account for was the provider's word
    a.limit_reached = row.str_at("status") == Some("rejected") && a.status() != "rejected";
    a
}

fn model_of(m: &Value, seen: i64) -> Option<ModelWindow> {
    Some(ModelWindow {
        label: m.str_at("label")?.to_string(),
        minutes: m.get("window_minutes").and_then(Value::i64).unwrap_or(WEEK_MINUTES),
        window: Window {
            util: f64_at(m, "utilization")?,
            reset: m.get("reset").and_then(Value::i64),
            severity: m.str_at("severity").map(str::to_string),
            is_active: m.get("is_active").and_then(Value::bool),
            seen,
        },
    })
}

fn credits_of(c: &Value) -> Credits {
    Credits {
        enabled: c.get("enabled").and_then(Value::bool),
        unlimited: c.get("unlimited").and_then(Value::bool),
        balance: f64_at(c, "balance"),
        used: f64_at(c, "used"),
        limit: f64_at(c, "limit"),
        utilization: f64_at(c, "utilization"),
        disabled_reason: c.str_at("disabled_reason").map(str::to_string),
        unit: c.str_at("unit").map(str::to_string),
        observed_at: c.get("observed_at").and_then(Value::i64),
        spend: c.str_at("spend").map(str::to_string),
        spending: c.get("spending").and_then(Value::bool),
        spent_24h: f64_at(c, "spent_24h"),
        rate_per_h: f64_at(c, "rate_per_h"),
        expiry_source: c.str_at("expiry_source").map(str::to_string),
        cap: c.get("cap").filter(|v| **v != Value::Null).map(|v| Cap {
            reached: v.get("reached").and_then(Value::bool),
            limit: f64_at(v, "limit"),
            remaining_percent: f64_at(v, "remaining_percent"),
            reached_type: v.str_at("reached_type").map(str::to_string),
        }),
        drops: c.get("unexplained_drops").filter(|v| **v != Value::Null).map(|v| {
            v.arr().iter().filter_map(|d| Some((d.get("at").and_then(Value::i64)?, f64_at(d, "amount")?))).collect()
        }),
    }
}
