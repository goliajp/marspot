//! How far finished scans of the shared rollouts have read for each
//! account they found no balance for, kept in `credit_scan.tsv`: derived,
//! rewritten whole, and deleting it costs one full scan again.
//!
//! A row is the account id, the newest day read (`YYYYMMDD`, the name of
//! its directory) and `1`, the scan having finished; a scan that did not
//! finish writes nothing.  The next scan for the account starts at that
//! day: read again, since it may have been today and written to since,
//! and every older day is left alone.

use std::fs;
use std::path::PathBuf;

use super::alloc_per_point::write_atomic;
use super::credit_lots::{civil, days_of};
use crate::plugins::claudecode::allocation_events::DAY;

/// More accounts than one machine logs in to; one past it is scanned in
/// full each time.
const ROWS_KEPT: usize = 64;

pub(super) struct ScanProgress {
    path: PathBuf,
    /// Account id and the start of the newest day read, in UTC.
    rows: Vec<(String, i64)>,
}

impl ScanProgress {
    pub(super) fn load(path: PathBuf) -> ScanProgress {
        let rows = fs::read_to_string(&path).unwrap_or_default().lines().filter_map(parse).take(ROWS_KEPT).collect();
        ScanProgress { path, rows }
    }

    /// The start of the first day a scan for `account` reads.
    pub(super) fn from(&self, account: &str) -> Option<i64> {
        self.rows.iter().find(|(a, _)| a == account).map(|(_, day)| *day)
    }

    /// A finished scan read `account`'s days through `day`; only ever
    /// moves on.  Whether the rows changed.
    pub(super) fn advance(&mut self, account: &str, day: i64) -> bool {
        if let Some((_, d)) = self.rows.iter_mut().find(|(a, _)| a == account) {
            let moved = *d < day;
            *d = (*d).max(day);
            return moved;
        }
        let room = self.rows.len() < ROWS_KEPT;
        if room {
            self.rows.push((account.to_string(), day));
        }
        room
    }

    pub(super) fn save(&self) -> std::io::Result<()> {
        let body: String = self.rows.iter().map(|(a, day)| format!("{a}\t{}\t1\n", name(*day))).collect();
        write_atomic(&self.path, &body)
    }
}

/// The directory name of the day starting at `day`.
fn name(day: i64) -> String {
    let (y, m, d) = civil(day.div_euclid(DAY));
    format!("{y:04}{m:02}{d:02}")
}

fn parse(line: &str) -> Option<(String, i64)> {
    let [account, day, "1"] = line.split('\t').collect::<Vec<_>>()[..] else { return None };
    if account.is_empty() || day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d) = (day[..4].parse().ok()?, day[4..6].parse().ok()?, day[6..].parse().ok()?);
    Some((account.to_string(), days_of(y, m, d) * DAY))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::test_env::TempDir;

    #[test]
    fn rows_only_move_on_and_read_back() {
        let t = TempDir::new("codex-scan-progress");
        let path = t.join("credit_scan.tsv");
        let mut p = ScanProgress::load(path.clone());
        let oct_1 = days_of(2026, 10, 1) * DAY;
        assert!(p.advance("acct-a", oct_1));
        assert!(!p.advance("acct-a", oct_1 - DAY), "never back");
        p.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "acct-a\t20261001\t1\n");
        assert_eq!(ScanProgress::load(path.clone()).from("acct-a"), Some(oct_1));
        // a row of a scan that did not finish, or broken, is not taken
        fs::write(&path, "acct-a\t20261001\t0\nacct-b\t2026101\t1\n").unwrap();
        let p = ScanProgress::load(path);
        assert_eq!((p.from("acct-a"), p.from("acct-b")), (None, None));
    }
}
