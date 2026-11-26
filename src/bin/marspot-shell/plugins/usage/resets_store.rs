//! Every reading of an account's stored resets, as facts, and the file
//! of the last one of each account.
//!
//! `usage/resets/YYYYMMDD.tsv`, append-only, tab-separated, a header at
//! the top of each file: `observed_at agent profile source outcome
//! block`, `block` being the reading in the shape of `usage/resets.json`
//! on one line.  A reading is written when it says something the last
//! one of that account did not; one that says the same again adds
//! nothing.  One file per UTC day, fifteen days kept.  `profile` is the
//! account's number, never its address, and the block has no grant id.
//!
//! `usage/resets.json` is derived: the last reading of each account,
//! rewritten whole after each pass whose readings were all recorded.
//! It is never read back here; a restart starts from the facts.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use marspot::agent_resets::{self, AccountResets};

use crate::plugins::claudecode::allocation_events::{prune_days, ymd};

pub(super) const HEADER: &str = "observed_at\tagent\tprofile\tname\tsource\toutcome\tblock\n";

pub(super) struct ResetStore {
    dir: PathBuf,
    file: PathBuf,
    /// The last reading of each account seen, written or not.
    last: Vec<AccountResets>,
    /// The last reading written as a fact, by `(agent, profile)`.
    written: HashMap<(String, u8), agent_resets::Resets>,
    pruned_day: Option<String>,
}

impl ResetStore {
    /// `usage` is the directory of the feeds.  What the facts already
    /// hold is not new: the last fact of each account is both what was
    /// written and what `resets.json` says until the next pass.  Day
    /// files past fifteen days go now, whether or not anything is written
    /// today.
    pub(super) fn new(usage: PathBuf, now: i64) -> ResetStore {
        let dir = usage.join("resets");
        let last = rebuild(&dir);
        let written = last.iter().map(|a| ((a.agent.clone(), a.profile), a.resets.clone())).collect();
        let mut store = ResetStore { dir, file: usage.join("resets.json"), last, written, pruned_day: None };
        if let Err(e) = store.prune(now) {
            marspot_term::lx_warn!("usage.resets.prune_failed", &e.to_string());
        }
        store
    }

    /// Remove day files past fifteen days, once per UTC day.
    fn prune(&mut self, now: i64) -> std::io::Result<()> {
        let today = ymd(now);
        if self.pruned_day.as_deref() == Some(today.as_str()) || !self.dir.is_dir() {
            return Ok(());
        }
        prune_days(&self.dir, now)?;
        self.pruned_day = Some(today);
        Ok(())
    }

    /// Take one pass's readings: append those that say something new and
    /// replace each account's last.  The number of facts appended.  A
    /// reading counts as written only once its day file took it.
    pub(super) fn record(&mut self, readings: Vec<AccountResets>, now: i64) -> std::io::Result<usize> {
        let mut text: Vec<(String, String)> = Vec::new();
        let mut fresh = Vec::new();
        for a in &readings {
            let key = (a.agent.clone(), a.profile);
            if self.written.get(&key).is_some_and(|w| w.same_as(&a.resets)) {
                continue;
            }
            let day = ymd(a.resets.observed_at);
            match text.iter_mut().find(|(d, _)| *d == day) {
                Some((_, t)) => t.push_str(&line(a)),
                None => text.push((day, line(a))),
            }
            fresh.push((key, a.resets.clone()));
        }
        for a in readings {
            match self.last.iter_mut().find(|l| l.agent == a.agent && l.profile == a.profile) {
                Some(l) => *l = a,
                None => self.last.push(a),
            }
        }
        self.prune(now)?;
        if fresh.is_empty() {
            return Ok(0);
        }
        fs::create_dir_all(&self.dir)?;
        for (day, body) in text {
            // one writer: the usage collector's thread
            let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{day}.tsv")))?;
            let header = if f.metadata()?.len() == 0 { HEADER } else { "" };
            f.write_all(format!("{header}{body}").as_bytes())?;
        }
        let n = fresh.len();
        self.written.extend(fresh);
        Ok(n)
    }

    /// Rewrite `resets.json` from the last reading of each account.
    pub(super) fn write(&self, now: i64) -> std::io::Result<()> {
        super::feed::write_atomic(&self.file, &agent_resets::render(&self.last, now))
    }

    #[cfg(test)]
    pub(super) fn last(&self) -> &[AccountResets] {
        &self.last
    }
}

fn line(a: &AccountResets) -> String {
    let r = &a.resets;
    let block = r.to_value(&format!("{}:{}", a.agent, a.profile)).to_json();
    let name = a.name.replace(['\t', '\n'], " ");
    format!("{}\t{}\t{}\t{name}\t{}\t{}\t{block}\n", r.observed_at, a.agent, a.profile, r.source, r.outcome.as_str())
}

/// The last fact of each account in the day files under `dir`: what
/// `resets.json` says, had it been lost.
pub(super) fn rebuild(dir: &std::path::Path) -> Vec<AccountResets> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.sort();
    let mut out: Vec<AccountResets> = Vec::new();
    for row in files.iter().flat_map(|f| fs::read_to_string(f).unwrap_or_default().lines().skip(1).map(str::to_string).collect::<Vec<_>>()) {
        let cols: Vec<&str> = row.splitn(7, '\t').collect();
        let (Some(agent), Some(profile), Some(name), Some(block)) = (cols.get(1), cols.get(2).and_then(|p| p.parse().ok()), cols.get(3), cols.get(6)) else { continue };
        let Some(resets) = marspot_peer::json::parse(block).ok().as_ref().and_then(agent_resets::Resets::from_value) else { continue };
        out.retain(|a| !(a.agent == *agent && a.profile == profile));
        out.push(AccountResets { agent: agent.to_string(), profile, name: name.to_string(), resets });
    }
    out
}
