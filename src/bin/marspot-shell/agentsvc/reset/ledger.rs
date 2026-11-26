//! Every claim attempt as a fact, the answer each request id got, and
//! which accounts have a claim running.
//!
//! `usage/reset_claims/YYYYMMDD.tsv`, append-only, one file per UTC
//! day, fifteen days kept, a header at the top of each file.  A row is
//! written for every attempt, refused ones included; no token, key or
//! grant id is ever written.  The answers of the last
//! [`MAX_ANSWERS`] finished requests are kept in memory, rebuilt from
//! the facts at start, so a request id asked again gets its first
//! answer and nothing is claimed twice.  An attempt marked `again` is
//! not an answer: its confirmed claim is kept in memory only, so the
//! same request asked again before a restart tries again without a
//! second confirmation; after a restart it must be confirmed anew.

use std::collections::VecDeque;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marspot_peer::json::{self, Value};

use super::{Claim, Grant, Outcome, Res};
use crate::plugins::claudecode::allocation_events::{prune_days, ymd};

pub(super) const HEADER: &str = "at\trequest_id\tprovider\tprofile\tkind\tgrant_ref\tby\tresult\tleft_after\tdetail\treason\tagain\tobserved_after\n";

/// Answers kept for asking again; far more than claims in fifteen days.
const MAX_ANSWERS: usize = 512;
/// Confirmed requests that may be tried again; one per account is all
/// a working machine has.
const MAX_AGAIN: usize = 32;

/// One finished attempt, as the facts hold it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    pub at: i64,
    pub request_id: String,
    pub provider: String,
    pub profile: u8,
    pub kind: String,
    pub by: String,
    pub outcome: Outcome,
}

pub(super) struct Ledger {
    dir: PathBuf,
    answers: VecDeque<Row>,
    /// `(provider, profile, request id)` of each claim running.
    running: Vec<(String, u8, String)>,
    /// Each request whose last attempt may be tried again, as it was
    /// confirmed.
    again: VecDeque<Claim>,
}

/// Holds an account for one claim; dropping it lets the next one in.
pub(super) struct Hold {
    ledger: Arc<Mutex<Ledger>>,
    key: (String, u8),
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        l.running.retain(|(p, n, _)| (p.as_str(), *n) != (self.key.0.as_str(), self.key.1));
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

impl Ledger {
    /// `dir` is `usage/reset_claims`.  Day files past fifteen days go now.
    pub(super) fn open(dir: PathBuf, now: i64) -> Ledger {
        if dir.is_dir()
            && let Err(e) = prune_days(&dir, now)
        {
            marspot::lx_warn!("svc.reset.prune_failed", &e.to_string());
        }
        let mut rows = rebuild(&dir);
        rows.retain(|r| r.outcome.is_final());
        let skip = rows.len().saturating_sub(MAX_ANSWERS);
        Ledger { dir, answers: rows.into_iter().skip(skip).collect(), running: Vec::new(), again: VecDeque::new() }
    }

    /// The answer `request_id` already got, if it finished.
    pub(super) fn answered(&self, request_id: &str) -> Option<&Row> {
        self.answers.iter().rev().find(|r| r.request_id == request_id)
    }

    /// The grant `c`'s request was confirmed for, when its last attempt
    /// may be tried again and `c` asks for the same account and grant.
    pub(super) fn confirmed_before(&self, c: &Claim) -> Option<Grant> {
        let same = |k: &&Claim| {
            (k.request_id.as_str(), k.provider, k.profile, k.grant.kind.as_str(), k.grant.grant_ref.as_str())
                == (c.request_id.as_str(), c.provider, c.profile, c.grant.kind.as_str(), c.grant.grant_ref.as_str())
        };
        self.again.iter().find(same).map(|k| k.grant.clone())
    }

    /// Every finished attempt kept, oldest first.
    pub(super) fn rows(&self) -> impl Iterator<Item = &Row> {
        self.answers.iter()
    }

    /// Take the account of `c` for this claim; `None` when a claim on it
    /// or with the same request id is running.
    pub(super) fn hold(ledger: &Arc<Mutex<Ledger>>, c: &Claim) -> Option<Hold> {
        let mut l = ledger.lock().unwrap_or_else(|p| p.into_inner());
        let provider = c.provider.as_str().to_string();
        if l.running.iter().any(|(p, n, id)| (*p == provider && *n == c.profile) || *id == c.request_id) {
            return None;
        }
        l.running.push((provider.clone(), c.profile, c.request_id.clone()));
        Some(Hold { ledger: ledger.clone(), key: (provider, c.profile) })
    }

    /// Write the attempt down; a finished one is also kept as the answer
    /// to its request id.
    pub(super) fn record(&mut self, c: &Claim, o: &Outcome, now: i64) -> std::io::Result<()> {
        let row = Row {
            at: now,
            request_id: c.request_id.clone(),
            provider: c.provider.as_str().into(),
            profile: c.profile,
            kind: c.grant.kind.clone(),
            by: c.by.as_str().into(),
            outcome: o.clone(),
        };
        let left = o.left_after.map_or_else(String::new, |n| n.to_string());
        let observed = o.observed_after.as_ref().map_or_else(String::new, Value::to_json);
        let line = format!(
            "{now}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{left}\t{}\t{}\t{}\t{observed}\n",
            clean(&c.request_id),
            row.provider,
            c.profile,
            clean(&c.grant.kind),
            clean(&c.grant.grant_ref),
            row.by,
            o.result.as_str(),
            clean(&o.detail),
            clean(&c.reason),
            if o.again { "1" } else { "" },
        );
        self.again.retain(|k| k.request_id != c.request_id);
        if o.again {
            if self.again.len() >= MAX_AGAIN {
                self.again.pop_front();
            }
            self.again.push_back(c.clone());
        }
        if o.is_final() {
            if self.answers.len() >= MAX_ANSWERS {
                self.answers.pop_front();
            }
            self.answers.push_back(row);
        }
        prune_days(&self.dir, now).ok();
        fs::create_dir_all(&self.dir)?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{}.tsv", ymd(now))))?;
        let header = if f.metadata()?.len() == 0 { HEADER } else { "" };
        f.write_all(format!("{header}{line}").as_bytes())
    }
}

/// Every row of the day files under `dir`, oldest first.
pub(super) fn rebuild(dir: &std::path::Path) -> Vec<Row> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.sort();
    let text: Vec<String> = files.iter().map(|f| fs::read_to_string(f).unwrap_or_default()).collect();
    text.iter().flat_map(|t| t.lines().skip(1)).filter_map(row_of).collect()
}

fn row_of(line: &str) -> Option<Row> {
    let c: Vec<&str> = line.splitn(13, '\t').collect();
    if c.len() < 13 {
        return None;
    }
    let outcome = Outcome {
        result: Res::parse(c[7])?,
        left_after: c[8].parse().ok(),
        observed_after: json::parse(c[12]).ok(),
        detail: c[9].to_string(),
        again: c[11] == "1",
    };
    Some(Row { at: c[0].parse().ok()?, request_id: c[1].into(), provider: c[2].into(), profile: c[3].parse().ok()?, kind: c[4].into(), by: c[6].into(), outcome })
}
