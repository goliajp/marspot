//! The Kimi usage feed marspot collects, and the accounts panel's rows
//! for the agents that are not Claude.
//!
//! The feed has the schema of the Claude one: Kimi meters an account
//! by a five-hour and a seven-day window, as Claude does.

use std::path::PathBuf;

use crate::cc_usage::{self, CcUsage, CodexUsage, CxAccount};

/// Where marspot writes the Kimi feed it collects.
pub fn own_feed_path() -> PathBuf {
    marspot_term::paths::state_root().join("usage/kimi-usage.json")
}

/// What the feed calls the account of profile `n`; 0 is the default
/// home.
pub fn account_name(n: u8) -> String {
    if n == 0 { "Kimi".into() } else { format!("Kimi {n}") }
}

/// Read + parse the Kimi feed. `None` on missing file / parse failure.
pub fn read() -> Option<CcUsage> {
    cc_usage::parse(&std::fs::read_to_string(own_feed_path()).ok()?)
}

/// The Kimi accounts as panel rows.
fn rows(usage: CcUsage) -> CodexUsage {
    let accounts = usage
        .accounts
        .into_iter()
        .map(|a| CxAccount { windows: a.windows(), name: a.name, email: a.email, status: a.status })
        .collect();
    CodexUsage { generated_at: usage.generated_at, accounts }
}

/// Two feeds as one list, stamped with the older generation: the stamp
/// speaks for every row under it.
fn merged(codex: Option<CodexUsage>, kimi: Option<CodexUsage>) -> Option<CodexUsage> {
    match (codex, kimi) {
        (Some(mut c), Some(k)) => {
            c.generated_at = c.generated_at.min(k.generated_at);
            c.accounts.extend(k.accounts);
            Some(c)
        }
        (c, k) => c.or(k),
    }
}

/// The Codex accounts, then the Kimi ones, for the accounts panel.
pub fn read_codex_and_kimi() -> Option<CodexUsage> {
    merged(cc_usage::read_codex(), read().map(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{"generated_at":"2026-11-10T05:55:14+00:00","accounts":[
      {"name":"Kimi 2","email":"","status":"rejected","utilization_5h":1,"utilization_7d":0.4,"reset_5h":1794300000,"reset_7d":1794800000,"model_limits":[],"credits":null,"tier":"","collected_at":"2026-11-10T05:55:14+00:00"},
      {"name":"Kimi","email":"ada@example.jp","status":"allowed","utilization_5h":null,"utilization_7d":0.25,"reset_5h":null,"reset_7d":1794700000,"model_limits":[],"credits":null,"tier":"","collected_at":"2026-11-10T05:50:00+00:00"}]}"#;

    #[test]
    fn a_kimi_account_is_a_five_hour_and_a_seven_day_row() {
        let k = rows(cc_usage::parse(FEED).unwrap());
        let names: Vec<&str> = k.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Kimi", "Kimi 2"]);
        let two = &k.accounts[1];
        let got: Vec<(&str, f64, Option<i64>)> = two.windows.iter().map(|w| (w.label.as_str(), w.util, w.reset)).collect();
        assert_eq!(got, [("5h", 1.0, Some(1_794_300_000)), ("7d", 0.4, Some(1_794_800_000))]);
        assert_eq!(two.status, "rejected");
        let one = &k.accounts[0];
        assert_eq!((one.email.as_str(), one.windows[0].reset, one.windows[1].util), ("ada@example.jp", None, 0.25));
        assert_eq!((account_name(0), account_name(2)), ("Kimi".to_string(), "Kimi 2".to_string()));
    }

    #[test]
    fn the_panel_lists_codex_then_kimi_under_the_older_stamp() {
        let kimi = rows(cc_usage::parse(FEED).unwrap());
        let codex = CodexUsage { generated_at: kimi.generated_at - 600, accounts: vec![kimi.accounts[0].clone()] };
        let both = merged(Some(codex.clone()), Some(kimi.clone())).unwrap();
        assert_eq!((both.generated_at, both.accounts.len()), (codex.generated_at, 3));
        assert_eq!(merged(None, Some(kimi.clone())), Some(kimi));
        assert_eq!(merged(Some(codex.clone()), None), Some(codex));
        assert_eq!(merged(None, None), None);
    }
}
