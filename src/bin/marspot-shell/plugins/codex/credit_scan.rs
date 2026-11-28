//! The earliest reading in the shared codex rollouts that shows each
//! account's paid balance: when its credits were first there to see.
//!
//! A rollout names the account that made it (`creator_account_id` in
//! its first line), the account id its login carries; rollouts from
//! before codex wrote that cannot be told apart and are skipped.  A
//! session resumed under another account goes on in the same file, so
//! a file's readings count for its maker only while their week renews
//! when the first one's does: a change is another account, or the next
//! week, and either way the rest of the file is left.  The first
//! reading with a balance above zero ends a file.
//!
//! Days are read oldest first and the walk stops once every account has
//! a reading older than the day being opened; files of other makers are
//! read no further than their first line.  An account's search can
//! start at a later day (what an earlier finished scan read without
//! finding a balance is not read again).  The whole scan gives up at a
//! deadline and says it did not finish; the accounts of a directory read
//! to its end, or to the stop above, are settled.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_peer::json;

use super::alloc_rollout::{Note, notes_of};
use super::credit_lots::days_of;
use crate::plugins::claudecode::allocation_events::DAY;

/// How long one scan may take.
const SCAN_FOR: Duration = Duration::from_secs(600);

/// Readings this close in their week's renewal are the same week: the
/// records jitter by a second.
const SAME_WEEK_S: i64 = 60;

/// Larger than any login codex writes.
const MAX_LOGIN: u64 = 64 * 1024;

/// A line buffer grown past this is let go after the line.
const BUF_KEPT: usize = 1 << 20;

const MAKER: &str = r#""creator_account_id":""#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Found {
    pub at: i64,
    pub balance: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Want {
    pub profile: u8,
    pub account: String,
    /// The start of the first day to read; `i64::MIN` for all.
    pub from: i64,
}

/// What one scan found, and what it cost.
#[derive(Debug, Default)]
pub(super) struct Scanned {
    pub wants: Vec<Want>,
    pub found: HashMap<u8, Found>,
    /// The accounts whose directory was read to the end or to the stop,
    /// each with the start of the newest day opened, if any was.
    pub settled: HashMap<u8, Option<i64>>,
    pub complete: bool,
    /// Whether the scan ran at background priority.
    pub background: bool,
    pub files: u64,
    pub bytes: u64,
    pub secs: u64,
}

/// The account id of the login at `home`; `None` without one.  Nothing
/// else of the login is kept.
pub(super) fn account_of(home: &Path) -> Option<String> {
    let mut text = String::new();
    fs::File::open(home.join("auth.json")).ok()?.take(MAX_LOGIN).read_to_string(&mut text).ok()?;
    let id = json::parse(&text).ok()?.at(&["tokens", "account_id"])?.str()?.to_string();
    // it is matched inside other files' text: plain id characters only
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(id)
}

/// Each `(sessions directory, account)` scanned, one directory at a time.
pub(super) fn scan_all(wants: Vec<(PathBuf, Want)>) -> Scanned {
    let start = Instant::now();
    let mut out = Scanned { wants: wants.iter().map(|(_, w)| w.clone()).collect(), complete: true, ..Scanned::default() };
    let mut by_dir: Vec<(PathBuf, Vec<Want>)> = Vec::new();
    for (dir, w) in wants {
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        match by_dir.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, ws)) => ws.push(w),
            None => by_dir.push((dir, vec![w])),
        }
    }
    for (dir, ws) in by_dir {
        out.complete &= scan(&dir, &ws, start + SCAN_FOR, &mut out);
    }
    out.secs = start.elapsed().as_secs();
    out
}

/// Put the calling thread at background priority: its CPU time and its
/// disk reads yield to everything else.  Whether it took.
pub(super) fn background_thread() -> bool {
    // SAFETY: with PRIO_DARWIN_THREAD and who 0 the call changes only
    // the calling thread
    unsafe { libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG) == 0 }
}

/// The day directories under `sessions`, oldest first, each with the
/// moment its day starts in UTC.
fn days(sessions: &Path) -> Vec<(i64, PathBuf)> {
    let sorted = |p: &Path| {
        let mut v: Vec<PathBuf> = fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        v.sort();
        v
    };
    let num = |p: &Path| p.file_name()?.to_str()?.parse::<i64>().ok();
    let mut out = Vec::new();
    for y in sorted(sessions) {
        for m in sorted(&y) {
            for d in sorted(&m) {
                let (Some(yy), Some(mm), Some(dd)) = (num(&y), num(&m), num(&d)) else { continue };
                out.push((days_of(yy, mm, dd) * DAY, d));
            }
        }
    }
    out
}

/// Scan one sessions directory for `wants`; false when the deadline
/// stopped it.
fn scan(sessions: &Path, wants: &[Want], deadline: Instant, out: &mut Scanned) -> bool {
    let mut buf = Vec::new();
    let first = wants.iter().map(|w| w.from).min().unwrap_or(i64::MIN);
    let mut opened = None;
    for (start, day) in days(sessions).into_iter().filter(|(start, _)| *start >= first) {
        // a day's directory holds sessions made that local day: none of
        // its readings is older than the day before in UTC
        let newest = wants.iter().map(|w| out.found.get(&w.profile).map(|f| f.at)).collect::<Option<Vec<_>>>();
        if newest.and_then(|v| v.into_iter().max()).is_some_and(|at| start - DAY > at) {
            break;
        }
        opened = Some(start);
        let mut files: Vec<PathBuf> = fs::read_dir(&day).into_iter().flatten().flatten().map(|e| e.path()).collect();
        files.retain(|p| p.extension().is_some_and(|x| x == "jsonl"));
        files.sort();
        for f in files {
            if Instant::now() > deadline {
                return false;
            }
            if !file(&f, wants, start, deadline, &mut buf, out) {
                return false;
            }
        }
    }
    for w in wants {
        out.settled.insert(w.profile, opened);
    }
    true
}

/// One rollout; false only when the deadline passed inside it.
fn file(path: &Path, wants: &[Want], day: i64, deadline: Instant, buf: &mut Vec<u8>, out: &mut Scanned) -> bool {
    let Ok(f) = fs::File::open(path) else { return true };
    out.files += 1;
    let mut r = BufReader::with_capacity(256 * 1024, f);
    let mut next = |buf: &mut Vec<u8>, out: &mut Scanned| {
        if buf.capacity() > BUF_KEPT {
            *buf = Vec::new();
        }
        buf.clear();
        let n = r.read_until(b'\n', buf).unwrap_or(0);
        out.bytes += n as u64;
        n > 0
    };
    if !next(buf, out) {
        return true;
    }
    let Some(profile) = maker(buf).and_then(|m| wants.iter().find(|w| w.from <= day && w.account.as_bytes() == m)).map(|w| w.profile) else { return true };
    let mut week: Option<i64> = None;
    let mut lines = 0u32;
    while next(buf, out) {
        lines = lines.wrapping_add(1);
        if lines.is_multiple_of(4096) && Instant::now() > deadline {
            return false;
        }
        if !contains(buf, br#""token_count""#) || (contains(buf, br#""limit_id":""#) && !contains(buf, br#""limit_id":"codex""#)) {
            continue;
        }
        let Ok(line) = std::str::from_utf8(buf) else { continue };
        for n in notes_of(line) {
            let Note::Limits { at, week: Some((_, renews)), balance, .. } = n else { continue };
            if out.found.get(&profile).is_some_and(|f| at >= f.at) || (*week.get_or_insert(renews) - renews).abs() > SAME_WEEK_S {
                return true;
            }
            if let Some(balance) = balance.filter(|b| *b > 0.0) {
                out.found.insert(profile, Found { at, balance });
                return true;
            }
        }
    }
    true
}

/// The account that made a rollout, from its first line.
fn maker(first: &[u8]) -> Option<&[u8]> {
    let at = find(first, MAKER.as_bytes())? + MAKER.len();
    let len = first[at..].iter().position(|b| *b == b'"')?;
    Some(&first[at..at + len])
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

#[cfg(test)]
#[path = "credit_scan_tests.rs"]
mod tests;
