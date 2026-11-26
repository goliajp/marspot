//! `alloc_hours/YYYYMM.tsv`: one row per hour with what the fleet used,
//! kept sixty days.
//!
//! The day files of `alloc_events` are deleted after fifteen days, and
//! a forecast by hour of day or of week needs weeks of hours.  Each row
//! is summed once, from the facts of its hour, at the first scan after
//! the hour ends, and never rewritten: once the facts it was summed from
//! are gone it is the only record of that hour.  Rows are appended, and
//! a month's file is deleted whole once its last day is sixty days old.
//!
//! `at\thour\tfable=<points> other=<points> busy_h=<pane-hours> panes=<n>`:
//! points are 1% of one account's week, at the prices of the day the
//! row is summed; busy pane-hours count the time each bound pane spent
//! between a `busy` and the next `rest`.
//!
//! An hour before this shell started with no fact at all may have had
//! no shell to see it, and is written with every field `-`: an unknown
//! hour must not read as an idle fleet.  A pane that turned busy before
//! this shell started counts busy only up to its last fact before the
//! start, until this shell sees it turn busy again.  The first shell to
//! write the files sums back `CATCH_UP` hours from the facts kept.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::allocation_events::{DAY, Event, EventLog, ymd};
use super::allocation_prices::{Tokens, weeks};

/// Days of hours kept.
pub(super) const KEEP_DAYS: i64 = 60;
const HOUR: i64 = 3600;
/// Hours summed in one scan when the shell was not running for a while,
/// or the first time.
const CATCH_UP: i64 = 48;
/// How far before an hour the facts are read to know which panes were
/// busy when it began.
const LEAD_S: i64 = 6 * HOUR;

/// One hour of the fleet.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Hour {
    pub at: i64,
    pub fable: f64,
    pub other: f64,
    pub busy_h: f64,
    pub panes: usize,
    /// A shell was up through it, or some fact falls in it.
    pub known: bool,
}

impl Hour {
    fn line(&self) -> String {
        if !self.known {
            return format!("{}\thour\tfable=- other=- busy_h=- panes=-\n", self.at);
        }
        format!("{}\thour\tfable={:.4} other={:.4} busy_h={:.3} panes={}\n", self.at, self.fable, self.other, self.busy_h, self.panes)
    }
}

#[derive(Debug)]
pub(super) struct Hours {
    dir: PathBuf,
    /// The start of the last hour written; `None` before the files are read.
    last: Option<i64>,
    /// The first scan of this shell, taken as when it started.
    started: Option<i64>,
}

impl Hours {
    pub(super) fn new(dir: PathBuf) -> Self {
        Hours { dir, last: None, started: None }
    }

    /// Sum and append every hour that ended since the last one written,
    /// from the facts in `events`, as of `now`.
    pub(super) fn tick(&mut self, events: &EventLog, now: i64) -> std::io::Result<usize> {
        let started = *self.started.get_or_insert(now);
        let done = now - now.rem_euclid(HOUR) - HOUR;
        let last = *self.last.get_or_insert_with(|| last_written(&self.dir).unwrap_or(done - CATCH_UP * HOUR));
        if last >= done {
            return Ok(0);
        }
        let from = (last + HOUR).max(done - (CATCH_UP - 1) * HOUR);
        let facts = events.read_since(from - LEAD_S, done + HOUR - 1);
        let rows = sum(&facts, from, done, started);
        fs::create_dir_all(&self.dir)?;
        prune(&self.dir, now)?;
        for h in &rows {
            let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{}.tsv", &ymd(h.at)[..6])))?;
            f.write_all(h.line().as_bytes())?;
        }
        self.last = Some(done);
        Ok(rows.len())
    }
}

/// Each hour from `from` to `to`, both starts, out of `facts` (which
/// begin early enough to know who was busy at `from`); the shell has
/// been up since `up_since`.
pub(super) fn sum(facts: &[Event], from: i64, to: i64, up_since: i64) -> Vec<Hour> {
    let mut rows: Vec<Hour> = (0..=(to - from) / HOUR).map(|k| from + k * HOUR).map(|at| Hour { at, known: at + HOUR > up_since, ..Hour::default() }).collect();
    let mut seen: Vec<HashSet<u64>> = vec![HashSet::new(); rows.len()];
    let slot = |at: i64| (at >= from && at < to + HOUR).then(|| ((at - from) / HOUR) as usize);
    // the old shell's last sign of life: a span busy from before the start
    // counts to there, and again from the start on (the new shell restores
    // the pane's state without writing a fact for it)
    let old_last = facts.iter().filter(|e| e.at < up_since).map(|e| e.at).max();
    let span = |rows: &mut [Hour], s: i64, e: i64| {
        if s < up_since {
            add_busy(rows, s, old_last.unwrap_or(s).clamp(s, e));
            add_busy(rows, up_since, e);
        } else {
            add_busy(rows, s, e);
        }
    };
    let mut busy_since: HashMap<u64, i64> = HashMap::new();
    for e in facts {
        if let Some(k) = slot(e.at) {
            rows[k].known = true;
            if e.sid != 0 && e.profile.is_some() {
                seen[k].insert(e.sid);
            }
        }
        match e.kind.as_str() {
            "used" => {
                let model = e.detail.split(' ').find_map(|w| w.strip_prefix("model="));
                if let (Some(k), Some(m), Some(t)) = (slot(e.at), model, Tokens::parse(&e.detail)) {
                    let pts = weeks(m, &t) * 100.0;
                    if e.class == "fable" { rows[k].fable += pts } else { rows[k].other += pts }
                }
            }
            "busy" => {
                busy_since.entry(e.sid).or_insert(e.at);
            }
            // a pane moved to another account goes on being busy
            "rest" | "bound" if e.kind == "rest" || e.profile.is_none() => {
                if let Some(s) = busy_since.remove(&e.sid) {
                    span(&mut rows, s, e.at);
                }
            }
            _ => {}
        }
    }
    for s in busy_since.into_values() {
        span(&mut rows, s, to + HOUR);
    }
    for (r, s) in rows.iter_mut().zip(seen) {
        r.panes = s.len();
    }
    rows
}

fn add_busy(rows: &mut [Hour], a: i64, b: i64) {
    for r in rows.iter_mut() {
        let (lo, hi) = (a.max(r.at), b.min(r.at + HOUR));
        if hi > lo {
            r.busy_h += (hi - lo) as f64 / HOUR as f64;
        }
    }
}

/// The start of the newest hour in the files.
fn last_written(dir: &Path) -> Option<i64> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| month_of(p).is_some()).collect();
    names.sort();
    let text = fs::read_to_string(names.last()?).ok()?;
    text.lines().rev().find_map(|l| l.split('\t').next()?.parse().ok())
}

fn month_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".tsv")?;
    (name.len() == 6 && name.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

/// Delete the month files whose every day is older than `KEEP_DAYS`.
fn prune(dir: &Path, now: i64) -> std::io::Result<()> {
    let oldest_kept = &ymd(now - KEEP_DAYS * DAY)[..6];
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if month_of(&path).is_some_and(|m| m < oldest_kept) {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "allocation_hours_tests.rs"]
mod tests;
