//! `usage_samples/YYYYMMDD.tsv`: the readings marspot collected itself,
//! as facts.
//!
//! Only a provider whose feed marspot writes gets rows: Claude and
//! Codex while the devops feed is stale or missing, Kimi always (devops
//! does not collect it).  While devops collects, its own store is the
//! history and nothing is written here, with one exception: a Codex
//! account's `wallet` row, its paid balance as marspot read it itself
//! (the app-server, or the account's rollouts, which cost no request),
//! is written whatever collects; `used` is the balance and
//! `limit_or_pct` its unit.  A balance that did not read has no row.
//!
//! Append-only, tab-separated, a header line at the top of each file:
//! `collected_at agent profile window used limit_or_pct resets_at`.
//! One row per window observation: a window is written again only when
//! it was read again, so a pass that read nothing new adds nothing.
//! `profile` is the account's number (0 for the unnumbered home), never
//! its email.  Percent windows carry `used` in percent and `pct`; the
//! extra-usage spend carries `used` and its limit.  One file per UTC day
//! of `collected_at`, fifteen days kept, older files deleted whole.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use super::feed::{Account, Window};
use crate::plugins::claudecode::allocation_events::{prune_days, ymd, DAY, KEEP_DAYS};

pub(super) const HEADER: &str = "collected_at\tagent\tprofile\twindow\tused\tlimit_or_pct\tresets_at\n";

pub(super) struct Samples {
    dir: PathBuf,
    /// When each `(agent, profile, window)` was last written; only the
    /// windows of the accounts in the last pass are kept.
    last: HashMap<(&'static str, u8, String), i64>,
    /// When each `(agent, profile)` balance was last written; read from
    /// the newest day files at the first wallet pass.
    last_wallet: Option<HashMap<(&'static str, u8), i64>>,
    pruned_day: Option<String>,
}

/// One account's paid balance as marspot read it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Balance {
    pub name: String,
    pub profile: u8,
    pub at: i64,
    pub balance: f64,
    pub unit: &'static str,
}

struct Row {
    profile: u8,
    window: String,
    seen: i64,
    used: String,
    limit: String,
    reset: Option<i64>,
}

impl Samples {
    /// `held` is what the last run wrote to the feeds: those readings
    /// were sampled then and are not new.
    pub(super) fn new(dir: PathBuf, held: [(&'static str, &[Account]); 3]) -> Samples {
        let mut last = HashMap::new();
        for (agent, accounts) in held {
            for r in accounts.iter().flat_map(rows) {
                last.insert((agent, r.profile, r.window), r.seen);
            }
        }
        Samples { dir, last, last_wallet: None, pruned_day: None }
    }

    /// Append the windows of `accounts` read since the last call; the
    /// number of rows written.  A row counts as written only once its
    /// day file took it, so a failed write is tried again next pass.
    pub(super) fn record(&mut self, agent: &'static str, accounts: &[Account], now: i64) -> std::io::Result<usize> {
        let all: Vec<Row> = accounts.iter().flat_map(rows).collect();
        self.last.retain(|k, _| k.0 != agent || all.iter().any(|r| r.profile == k.1 && r.window == k.2));
        let new: Vec<Row> = all.into_iter().filter(|r| self.last.get(&(agent, r.profile, r.window.clone())) != Some(&r.seen)).collect();
        let keys = self.write(agent, new, now)?;
        let written = keys.len();
        self.last.extend(keys);
        Ok(written)
    }

    /// Append a `wallet` row for each balance read since the last one
    /// written; the number of rows written.
    pub(super) fn record_wallet(&mut self, agent: &'static str, balances: &[Balance], now: i64) -> std::io::Result<usize> {
        let dir = self.dir.clone();
        let last = self.last_wallet.get_or_insert_with(|| written_wallets(&dir, now));
        let new: Vec<Row> = balances
            .iter()
            .filter(|b| last.get(&(agent, b.profile)).is_none_or(|was| b.at > *was))
            .map(|b| Row { profile: b.profile, window: "wallet".into(), seen: b.at, used: format!("{:.6}", b.balance), limit: b.unit.into(), reset: None })
            .collect();
        let keys = self.write(agent, new, now)?;
        let last = self.last_wallet.get_or_insert_with(HashMap::new);
        for ((agent, profile, _), at) in &keys {
            last.insert((*agent, *profile), *at);
        }
        Ok(keys.len())
    }

    /// Append `rows` to their day files; the keys of those written.  A
    /// row counts as written only once its day file took it.
    fn write(&mut self, agent: &'static str, rows: Vec<Row>, now: i64) -> std::io::Result<Vec<((&'static str, u8, String), i64)>> {
        // a day already past retention would bring back a deleted file
        let oldest = ymd(now - (KEEP_DAYS - 1) * DAY);
        let mut by_day: Vec<Day> = Vec::new();
        for r in rows {
            let key = (agent, r.profile, r.window.clone());
            let day = ymd(r.seen);
            if day < oldest {
                continue;
            }
            let i = match by_day.iter().position(|d| d.day == day) {
                Some(i) => i,
                None => {
                    by_day.push(Day { day, text: String::new(), keys: Vec::new() });
                    by_day.len() - 1
                }
            };
            by_day[i].text.push_str(&line(agent, &r));
            by_day[i].keys.push((key, r.seen));
        }
        if by_day.is_empty() {
            return Ok(Vec::new());
        }
        fs::create_dir_all(&self.dir)?;
        let today = ymd(now);
        if self.pruned_day.as_deref() != Some(today.as_str()) {
            prune_days(&self.dir, now)?;
            self.pruned_day = Some(today);
        }
        let mut written = Vec::new();
        for Day { day, text, keys } in by_day {
            // the empty-file header check assumes one writer: the usage
            // collector thread is the only one
            let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{day}.tsv")))?;
            let header = if f.metadata()?.len() == 0 { HEADER } else { "" };
            f.write_all(format!("{header}{text}").as_bytes())?;
            written.extend(keys);
        }
        Ok(written)
    }
}

/// The new rows of one day file.
struct Day {
    day: String,
    text: String,
    keys: Vec<((&'static str, u8, String), i64)>,
}

fn line(agent: &str, r: &Row) -> String {
    let reset = r.reset.map_or_else(|| "-".to_string(), |t| t.to_string());
    format!("{}\t{agent}\t{}\t{}\t{}\t{}\t{reset}\n", r.seen, r.profile, r.window, r.used, r.limit)
}

/// The newest `wallet` row of each account in today's and yesterday's
/// files, so a restart does not write a balance again.
fn written_wallets(dir: &std::path::Path, now: i64) -> HashMap<(&'static str, u8), i64> {
    let mut out = HashMap::new();
    for day in [ymd(now - DAY), ymd(now)] {
        let text = fs::read_to_string(dir.join(format!("{day}.tsv"))).unwrap_or_default();
        for f in text.lines().map(|l| l.split('\t').collect::<Vec<_>>()).filter(|f| f.len() == 7 && f[3] == "wallet") {
            let agent: &'static str = match f[1] {
                "codex" => "codex",
                "claude" => "claude",
                "kimi" => "kimi",
                _ => continue,
            };
            if let (Ok(at), Ok(profile)) = (f[0].parse::<i64>(), f[2].parse::<u8>()) {
                let held = out.entry((agent, profile)).or_insert(at);
                *held = (*held).max(at);
            }
        }
    }
    out
}

/// `Claude 3` is 3; the unnumbered home (`Claude`, `Codex`, `Kimi`) is 0.
pub(super) fn profile_of(name: &str) -> Option<u8> {
    match name.rsplit_once(' ') {
        Some((_, n)) => n.parse().ok(),
        None => Some(0),
    }
}

fn rows(a: &Account) -> Vec<Row> {
    let Some(profile) = profile_of(&a.name) else { return Vec::new() };
    let pct = |window: String, w: &Window| Row {
        profile,
        window,
        seen: w.seen,
        used: format!("{:.2}", w.util * 100.0),
        limit: "pct".into(),
        reset: w.reset,
    };
    let mut out = Vec::new();
    out.extend(a.five_hour.as_ref().map(|w| pct("5h".into(), w)));
    out.extend(a.seven_day.as_ref().map(|w| pct("7d".into(), w)));
    for m in &a.models {
        let label = m.label.replace(['\t', '\n', ' '], "_");
        out.push(pct(format!("{label}/{}m", m.minutes), &m.window));
    }
    if let Some(used) = a.credits.as_ref().and_then(|c| c.used)
        && a.credits_seen > 0
    {
        let limit = a.credits.as_ref().and_then(|c| c.limit).map_or_else(|| "-".to_string(), |l| l.to_string());
        out.push(Row { profile, window: "credits".into(), seen: a.credits_seen, used: used.to_string(), limit, reset: None });
    }
    out
}
