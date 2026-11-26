//! One pass over the accounts' stored resets: Codex from what the
//! collector read itself or, while devops collects, from the devops
//! feed; Kimi, which has none, as an empty list.  Each reading goes to
//! the store; where an account was read both ways, whether the two
//! agree is logged when it changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use marspot::agent_resets::{AccountResets, Resets as Reading};
use marspot_agent::sched::reset::{Agreement, Count};
use marspot_term::{lx_info, lx_warn};

use super::codex_resets;
use super::feed::Account;
use super::resets_store::ResetStore;
use super::samples::profile_of;

pub(super) struct Resets {
    store: ResetStore,
    /// The last verdict logged for each account read both ways.
    agreed: HashMap<String, Agreement>,
}

impl Resets {
    /// `usage` is the directory of the feeds.
    pub(super) fn new(usage: PathBuf, now: i64) -> Resets {
        Resets { store: ResetStore::new(usage, now), agreed: HashMap::new() }
    }

    /// `own` is what the collector read from the Codex accounts itself,
    /// none while `devops` collects them; `devops_codex` is the devops
    /// Codex feed, fresh or not.
    pub(super) fn pass(&mut self, devops: bool, devops_codex: &Path, own: Vec<AccountResets>, kimi: &[Account], now: i64) {
        let feed = codex_resets::of_feed(&std::fs::read_to_string(devops_codex).unwrap_or_default());
        let mut readings = own;
        if devops {
            readings = feed.iter().filter_map(|(name, r)| codex_account(name, r.clone())).collect();
        } else {
            for (name, verdict, mine, theirs) in codex_resets::settle(&mut readings, &feed, now) {
                self.tell(name, verdict, mine, theirs);
            }
        }
        for a in kimi.iter().filter(|a| a.has_reading()) {
            let Some(profile) = profile_of(&a.name) else { continue };
            readings.push(AccountResets { agent: "kimi".into(), profile, name: a.name.clone(), resets: Reading::none(now, "none") });
        }
        if readings.is_empty() {
            return;
        }
        match self.store.record(readings, now) {
            Ok(0) => {}
            Ok(n) => lx_info!("usage.resets.recorded", "stored resets read", rows = n),
            Err(e) => {
                // the file is derived from the facts: none that failed to land
                lx_warn!("usage.resets.record_failed", &e.to_string());
                return;
            }
        }
        if let Err(e) = self.store.write(now) {
            lx_warn!("usage.resets.write_failed", &e.to_string());
        }
    }

    fn tell(&mut self, name: String, verdict: Agreement, mine: Count, theirs: Count) {
        if verdict == Agreement::Apart || self.agreed.get(&name) == Some(&verdict) {
            return;
        }
        let line = format!(
            "account={name} own_left={} feed_left={} own_next={} feed_next={}",
            mine.left,
            theirs.left,
            mine.next_expiry.unwrap_or(0),
            theirs.next_expiry.unwrap_or(0)
        );
        match verdict {
            Agreement::Differs => lx_warn!("usage.resets.disagree", &line),
            _ => lx_info!("usage.resets.agree", &line),
        }
        self.agreed.insert(name, verdict);
    }

    #[cfg(test)]
    pub(super) fn store(&self) -> &ResetStore {
        &self.store
    }
}

/// A Codex account of the devops feed as a reading of its own.
fn codex_account(name: &str, r: Reading) -> Option<AccountResets> {
    Some(AccountResets { agent: "codex".into(), profile: profile_of(name)?, name: name.to_string(), resets: r })
}

#[cfg(test)]
#[path = "resets_tests.rs"]
mod tests;
