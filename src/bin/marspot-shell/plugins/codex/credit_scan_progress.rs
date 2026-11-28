//! How far finished scans of the shared rollouts have read for each
//! profile they found no balance for, kept in `credit_scan.tsv`: derived,
//! rewritten whole, and deleting it costs one full scan again.
//!
//! A row is `P`, the profile number, the first 16 hex digits of the
//! SHA-256 of the profile's login account id, the newest day read
//! (`YYYYMMDD`, the name of its directory) and `1`, the scan having
//! finished; a scan that did not finish writes nothing.  The id itself is
//! never written: the digest only tells whether the profile has logged in
//! to another account since, and then its row is not taken.  The next
//! scan for the profile starts at that day: read again, since it may have
//! been today and written to since, and every older day is left alone.
//! Rows of any other shape (the older account-keyed ones) are dropped.

use std::fs;
use std::path::PathBuf;

use marspot_peer::control::sha256;

use super::alloc_per_point::write_atomic;
use super::credit_lots::{civil, days_of};
use crate::plugins::claudecode::allocation_events::DAY;

/// More profiles than one machine has; one past it is scanned in full
/// each time.
const ROWS_KEPT: usize = 64;

const DIGEST_HEX: usize = 16;

#[derive(Debug, PartialEq)]
struct Row {
    profile: u8,
    login: String,
    /// The start of the newest day read, in UTC.
    day: i64,
}

pub(super) struct ScanProgress {
    path: PathBuf,
    rows: Vec<Row>,
}

/// The digest a row keeps of `account`, the login's account id.
fn login_digest(account: &str) -> String {
    sha256::hex_prefix(account.as_bytes(), DIGEST_HEX)
}

impl ScanProgress {
    pub(super) fn load(path: PathBuf) -> ScanProgress {
        let rows = fs::read_to_string(&path).unwrap_or_default().lines().filter_map(parse).take(ROWS_KEPT).collect();
        ScanProgress { path, rows }
    }

    /// The start of the first day a scan for `profile`, logged in to
    /// `account`, reads; `None` when its row is of another login.
    pub(super) fn from(&self, profile: u8, account: &str) -> Option<i64> {
        let login = login_digest(account);
        self.rows.iter().find(|r| r.profile == profile && r.login == login).map(|r| r.day)
    }

    /// A finished scan read `profile`'s days through `day` for the login
    /// to `account`; only ever moves on, unless the login changed.
    /// Whether the rows changed.
    pub(super) fn advance(&mut self, profile: u8, account: &str, day: i64) -> bool {
        let login = login_digest(account);
        if let Some(r) = self.rows.iter_mut().find(|r| r.profile == profile) {
            if r.login != login {
                *r = Row { profile, login, day };
                return true;
            }
            let moved = r.day < day;
            r.day = r.day.max(day);
            return moved;
        }
        let room = self.rows.len() < ROWS_KEPT;
        if room {
            self.rows.push(Row { profile, login, day });
        }
        room
    }

    pub(super) fn save(&self) -> std::io::Result<()> {
        let body: String = self.rows.iter().map(|r| format!("P\t{}\t{}\t{}\t1\n", r.profile, r.login, name(r.day))).collect();
        write_atomic(&self.path, &body)
    }
}

/// The directory name of the day starting at `day`.
fn name(day: i64) -> String {
    let (y, m, d) = civil(day.div_euclid(DAY));
    format!("{y:04}{m:02}{d:02}")
}

fn parse(line: &str) -> Option<Row> {
    let ["P", profile, login, day, "1"] = line.split('\t').collect::<Vec<_>>()[..] else { return None };
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if login.len() != DIGEST_HEX || !hex(login) || day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d) = (day[..4].parse().ok()?, day[4..6].parse().ok()?, day[6..].parse().ok()?);
    Some(Row { profile: profile.parse().ok()?, login: login.to_string(), day: days_of(y, m, d) * DAY })
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::test_env::TempDir;

    #[test]
    fn rows_only_move_on_and_read_back_without_the_id() {
        let t = TempDir::new("codex-scan-progress");
        let path = t.join("credit_scan.tsv");
        let mut p = ScanProgress::load(path.clone());
        let oct_1 = days_of(2026, 10, 1) * DAY;
        assert!(p.advance(2, "acct-a", oct_1));
        assert!(!p.advance(2, "acct-a", oct_1 - DAY), "never back");
        p.save().unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(body, format!("P\t2\t{}\t20261001\t1\n", login_digest("acct-a")));
        assert!(!body.contains("acct-a"));
        let back = ScanProgress::load(path.clone());
        assert_eq!(back.from(2, "acct-a"), Some(oct_1));
        assert_eq!(back.from(3, "acct-a"), None, "keyed on the profile");
        assert_eq!(back.from(2, "acct-b"), None, "another login of the profile");
        // a row of a scan that did not finish, broken, or of the older account-keyed shape is not taken
        let d = login_digest("acct-a");
        fs::write(&path, format!("P\t2\t{d}\t20261001\t0\nP\t2\t{d}\t2026101\t1\nacct-a\t20261001\t1\nP\t2\tACCT-A\t20261001\t1\n")).unwrap();
        assert_eq!(ScanProgress::load(path).rows, vec![]);
    }

    #[test]
    fn a_new_login_of_the_profile_starts_its_row_again() {
        let t = TempDir::new("codex-scan-relogin");
        let mut p = ScanProgress::load(t.join("credit_scan.tsv"));
        let oct_1 = days_of(2026, 10, 1) * DAY;
        assert!(p.advance(1, "acct-a", oct_1));
        assert!(p.advance(1, "acct-b", oct_1 - DAY), "another account's days start over");
        assert_eq!((p.from(1, "acct-a"), p.from(1, "acct-b")), (None, Some(oct_1 - DAY)));
        assert_eq!(p.rows.len(), 1);
    }
}
