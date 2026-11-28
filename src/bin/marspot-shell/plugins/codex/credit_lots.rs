//! Each codex account's credit lot, kept in `credit_lots.tsv`: a fact
//! log, appended to and never rewritten.
//!
//! A line is one sighting of an account's balance: when it was first
//! seen, the account, the amount, its latest end, `latest`, whose
//! sighting it was (`rollout`, `devops`, `marspot`) and when the line
//! was written.  Lines of five fields predate the last two and were
//! marspot's own.  marspot writes its own first sighting once per
//! account, the moment local collection began; a scan of the shared
//! rollouts adds the earliest reading there that shows a balance, once,
//! and only from a scan that settled the account: a scan stopped by its
//! deadline may not have read the earliest days yet.  Where a settled
//! scan found nothing, `credit_scan_progress` keeps the day it reached.
//! Two logins with one account id (workspaces of one team) cannot be
//! told apart in the rollouts, and neither is scanned.
//!
//! The lot of an account is its line seen first; ties go to the rollout
//! before devops before marspot, then to the line written first, so the
//! choice does not hang on the order of the file.  The end is worked
//! out again from the sighting on every read, by the rule in force: a
//! grant seen before 2026-11-13 (Japan) lasts twelve months, a later
//! one 180 days.  The grant came no later than the sighting, so the end
//! is the latest it can be.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use marspot_agent::types::wallet::Lot;

use super::alloc::Log;
use super::credit_scan::{self, Found, Want};
use super::credit_scan_progress::ScanProgress;
use crate::plugins::claudecode::allocation_events::DAY;

/// 2026-11-13 00:00 in Japan: grants seen before it last twelve months.
const TWELVE_MONTHS_BEFORE: i64 = 1_794_495_600;

/// What a grant seen on or after that day lasts.
const JAPAN_LASTS_S: i64 = 180 * DAY;

/// Lines kept in memory; a few per account are written in its life.
const LINES_KEPT: usize = 1024;

/// Where an account's lot was first seen.
pub(super) const ROLLOUT: &str = "rollout";
pub(super) const MARSPOT: &str = "marspot";

#[derive(Clone, Debug, PartialEq)]
pub(super) struct LotLine {
    pub first_seen: i64,
    pub profile: u8,
    pub amount: f64,
    pub seen_by: String,
    pub recorded_at: i64,
}

impl LotLine {
    fn rank(&self) -> u8 {
        match self.seen_by.as_str() {
            ROLLOUT => 0,
            "devops" => 1,
            MARSPOT => 2,
            _ => 3,
        }
    }

    fn line(&self) -> String {
        let (first, p, amount, by, rec) = (self.first_seen, self.profile, self.amount, &self.seen_by, self.recorded_at);
        format!("{first}\t{p}\t{amount:.6}\t{}\tlatest\t{by}\t{rec}\n", expiry(first))
    }
}

/// When a grant first seen at `first_seen` ends at the latest.
pub(super) fn expiry(first_seen: i64) -> i64 {
    if first_seen < TWELVE_MONTHS_BEFORE { plus_a_year(first_seen) } else { first_seen + JAPAN_LASTS_S }
}

/// The same day and time a year on, in UTC; February 29 becomes the 28th.
fn plus_a_year(at: i64) -> i64 {
    let (days, secs) = (at.div_euclid(DAY), at.rem_euclid(DAY));
    let (y, m, d) = civil(days);
    let d = if m == 2 && d == 29 { 28 } else { d };
    days_of(y + 1, m, d) * DAY + secs
}

/// Year, month, day of a day count from 1970-01-01.
pub(super) fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub(super) fn days_of(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One line of `credit_lots.tsv`, of five fields or seven.
pub(super) fn parse(line: &str) -> Option<LotLine> {
    let f: Vec<&str> = line.split('\t').collect();
    let (first, profile, amount, by, rec) = match f[..] {
        [first, profile, amount, _, _] => (first, profile, amount, MARSPOT, first),
        [first, profile, amount, _, _, by, rec] => (first, profile, amount, by, rec),
        _ => return None,
    };
    let first_seen = first.parse().ok()?;
    Some(LotLine { first_seen, profile: profile.parse().ok()?, amount: amount.parse().ok()?, seen_by: by.to_string(), recorded_at: rec.parse().ok()? })
}

/// The lot among `lines` for `profile`.
pub(super) fn pick(lines: &[LotLine], profile: u8) -> Option<Lot> {
    let key = |l: &&LotLine| (l.first_seen, l.rank(), l.recorded_at, l.amount.to_bits());
    let l = lines.iter().filter(|l| l.profile == profile).min_by_key(key)?;
    Some(Lot { amount: l.amount, expires_at: expiry(l.first_seen), bound: "latest".into(), seen_by: Some(l.seen_by.clone()) })
}

pub(super) struct LotLog {
    path: PathBuf,
    /// Where the accounts' homes are: `.codex` and `.codex-profile-N`.
    homes: PathBuf,
    lines: Vec<LotLine>,
    scan: Option<JoinHandle<credit_scan::Scanned>>,
    progress: ScanProgress,
    /// Accounts scanned for, or found unscannable, by this process.
    scanned: HashSet<u8>,
}

impl LotLog {
    pub(super) fn new(path: PathBuf, homes: PathBuf) -> LotLog {
        let lines = fs::read_to_string(&path).unwrap_or_default().lines().filter_map(parse).take(LINES_KEPT).collect();
        let progress = ScanProgress::load(path.with_file_name("credit_scan.tsv"));
        LotLog { path, homes, lines, scan: None, progress, scanned: HashSet::new() }
    }

    pub(super) fn lot(&self, profile: u8) -> Option<Lot> {
        pick(&self.lines, profile)
    }

    fn has(&self, profile: u8, by: &str) -> bool {
        self.lines.iter().any(|l| l.profile == profile && l.seen_by == by)
    }

    fn write(&mut self, l: LotLine, log: &mut Log) {
        if let Err(e) = append(&self.path, &l.line()) {
            log.push(("codex.wallet.lots_failed", e.to_string()));
        }
        log.push(("codex.wallet.lot", l.line().trim_end().replace('\t', " ")));
        if self.lines.len() < LINES_KEPT {
            self.lines.push(l);
        }
    }

    /// marspot's own first sighting of `profile`'s balance, once.
    pub(super) fn sighted(&mut self, profile: u8, at: i64, balance: f64, now: i64, log: &mut Log) {
        if !self.has(profile, MARSPOT) {
            self.write(LotLine { first_seen: at, profile, amount: balance, seen_by: MARSPOT.into(), recorded_at: now }, log);
        }
    }

    /// Take a finished scan's findings, and start one for the accounts
    /// in `seen` the rollouts have not dated yet.  At most one scan runs.
    pub(super) fn scan(&mut self, seen: impl IntoIterator<Item = u8>, now: i64, log: &mut Log) {
        if self.scan.as_ref().is_some_and(JoinHandle::is_finished) {
            let done = self.scan.take().expect("a finished scan").join().unwrap_or_else(|e| std::panic::resume_unwind(e));
            self.took(done, now, log);
        }
        if self.scan.is_some() {
            return;
        }
        let fresh: Vec<u8> = seen.into_iter().filter(|p| !self.has(*p, ROLLOUT) && self.scanned.insert(*p)).collect();
        if fresh.is_empty() {
            return;
        }
        let ids = accounts(&self.homes);
        let mut wants: Vec<(PathBuf, Want)> = Vec::new();
        for profile in fresh {
            let Some((_, account)) = ids.iter().find(|(p, _)| *p == profile) else {
                log.push(("codex.wallet.lot_scan", format!("P{profile} no account id in its login; its rollouts cannot be told apart")));
                continue;
            };
            let twins: Vec<u8> = ids.iter().filter(|(p, a)| a == account && *p != profile).map(|(p, _)| *p).collect();
            if !twins.is_empty() {
                log.push(("codex.wallet.lot_scan", format!("ambiguous P{profile} shares its account id with {twins:?}; not scanned")));
                continue;
            }
            let from = self.progress.from(account).unwrap_or(i64::MIN);
            wants.push((home(&self.homes, profile).join("sessions"), Want { profile, account: account.clone(), from }));
        }
        if wants.is_empty() {
            return;
        }
        let spawned = std::thread::Builder::new().name("codex-credit-scan".into()).spawn(move || {
            let background = credit_scan::background_thread();
            credit_scan::Scanned { background, ..credit_scan::scan_all(wants) }
        });
        match spawned {
            Ok(h) => self.scan = Some(h),
            Err(e) => log.push(("codex.wallet.lot_scan", format!("not started: {e}"))),
        }
    }

    fn took(&mut self, done: credit_scan::Scanned, now: i64, log: &mut Log) {
        let profiles: Vec<u8> = done.wants.iter().map(|w| w.profile).collect();
        let (complete, files, bytes, secs, bg) = (done.complete, done.files, done.bytes, done.secs, done.background);
        let line = format!("profiles={profiles:?} complete={complete} files={files} bytes={bytes} secs={secs} background={bg}");
        log.push(("codex.wallet.lot_scan", line));
        let mut moved = false;
        for w in &done.wants {
            let Some(opened) = done.settled.get(&w.profile) else { continue };
            match done.found.get(&w.profile) {
                Some(&Found { at, balance }) if !self.has(w.profile, ROLLOUT) => {
                    self.write(LotLine { first_seen: at, profile: w.profile, amount: balance, seen_by: ROLLOUT.into(), recorded_at: now }, log);
                }
                Some(_) => {}
                None => moved |= opened.is_some_and(|day| self.progress.advance(&w.account, day)),
            }
        }
        if moved && let Err(e) = self.progress.save() {
            log.push(("codex.wallet.lot_scan_failed", e.to_string()));
        }
    }

    #[cfg(test)]
    pub(super) fn scanning(&self) -> bool {
        self.scan.as_ref().is_some_and(|h| !h.is_finished())
    }
}

/// The home of account `profile` under `homes`: `.codex-profile-N`,
/// `.codex` for 0.
fn home(homes: &Path, profile: u8) -> PathBuf {
    if profile == 0 { homes.join(".codex") } else { homes.join(format!(".codex-profile-{profile}")) }
}

/// The account id of every login under `homes`, by profile.
fn accounts(homes: &Path) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    for e in fs::read_dir(homes).into_iter().flatten().flatten() {
        let name = e.file_name();
        let profile = match name.to_str() {
            Some(".codex") => Some(0),
            Some(n) => n.strip_prefix(".codex-profile-").and_then(|n| n.parse::<u8>().ok()),
            None => None,
        };
        if let Some(account) = profile.and_then(|p| credit_scan::account_of(&home(homes, p)).map(|a| (p, a))) {
            out.push(account);
        }
    }
    out.sort();
    out
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(line.as_bytes())
}

#[cfg(test)]
#[path = "credit_lots_tests.rs"]
mod tests;
