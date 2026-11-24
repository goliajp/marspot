//! `alloc_events/YYYYMMDD.tsv`: what happened to each pane, as facts.
//!
//! Append-only, one line per change: `at agent sid kind profile class
//! detail`, tab-separated, `at` in unix seconds, `profile` a number or
//! `-`.  One file per UTC day; files older than fifteen days are
//! deleted whole, so the log stays bounded however long the shell
//! runs.  A restart reads the last fifteen hours back to rebuild what
//! it knew.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Days of files kept, today included.
pub(super) const KEEP_DAYS: i64 = 15;

pub(super) const DAY: i64 = 86_400;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Event {
    pub at: i64,
    pub sid: u64,
    pub kind: String,
    pub profile: Option<u8>,
    pub class: String,
    pub detail: String,
}

impl Event {
    fn line(&self) -> String {
        let profile = self.profile.map_or_else(|| "-".to_string(), |p| p.to_string());
        let clean = |s: &str| s.replace(['\t', '\n'], " ");
        format!("{}\tclaude\t{}\t{}\t{}\t{}\t{}\n", self.at, self.sid, self.kind, profile, self.class, clean(&self.detail))
    }

    fn parse(line: &str) -> Option<Event> {
        let mut f = line.splitn(7, '\t');
        let at = f.next()?.parse().ok()?;
        let _agent = f.next()?;
        let sid = f.next()?.parse().ok()?;
        let kind = f.next()?.to_string();
        let profile = match f.next()? {
            "-" => None,
            p => Some(p.parse().ok()?),
        };
        let class = f.next()?.to_string();
        let detail = f.next().unwrap_or("").to_string();
        Some(Event { at, sid, kind, profile, class, detail })
    }
}

/// The directory of day files.
#[derive(Debug)]
pub(super) struct EventLog {
    dir: PathBuf,
    pruned_day: Option<String>,
}

impl EventLog {
    pub(super) fn new(dir: PathBuf) -> Self {
        EventLog { dir, pruned_day: None }
    }

    pub(super) fn append(&mut self, ev: &Event) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let day = ymd(ev.at);
        if self.pruned_day.as_deref() != Some(day.as_str()) {
            self.prune(ev.at)?;
            self.pruned_day = Some(day.clone());
        }
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{day}.tsv")))?;
        f.write_all(ev.line().as_bytes())
    }

    /// Delete the day files older than `KEEP_DAYS` as of `now`.
    fn prune(&self, now: i64) -> std::io::Result<()> {
        let oldest = ymd(now - (KEEP_DAYS - 1) * DAY);
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if day_of(&path).is_some_and(|d| d < oldest.as_str()) {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// Every event at or after `from` up to `now`, oldest first.
    pub(super) fn read_since(&self, from: i64, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut day = from - from.rem_euclid(DAY);
        while day <= now {
            if let Ok(text) = fs::read_to_string(self.dir.join(format!("{}.tsv", ymd(day)))) {
                out.extend(text.lines().filter_map(Event::parse).filter(|e| e.at >= from && e.at <= now));
            }
            day += DAY;
        }
        out
    }
}

fn day_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".tsv")?;
    (name.len() == 8 && name.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

/// `YYYYMMDD` of unix seconds, UTC.
pub(super) fn ymd(secs: i64) -> String {
    let z = secs.div_euclid(DAY) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: i64, kind: &str) -> Event {
        Event { at, sid: 7, kind: kind.into(), profile: Some(3), class: "plain".into(), detail: "a\tb".into() }
    }

    #[test]
    fn allocation_events_dates_and_round_trip() {
        assert_eq!(ymd(0), "19700101");
        assert_eq!(ymd(1_795_530_644), "20261124");
        assert_eq!(ymd(951_782_400), "20000229");
        let e = ev(5, "bound");
        let back = Event::parse(e.line().trim_end()).unwrap();
        assert_eq!(back.detail, "a b", "a tab in the detail cannot split the line");
        assert_eq!(Event { detail: "a\tb".into(), ..back }, e);
        assert_eq!(Event::parse("x\tclaude\t1\tbound\t3\tplain\t"), None);
    }

    #[test]
    fn allocation_events_keep_fifteen_days() {
        let dir = std::env::temp_dir().join(format!("marspot-alloc-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut log = EventLog::new(dir.clone());
        let now = 1_795_530_644;
        for d in (0..20).rev() {
            log.append(&ev(now - d * DAY, "rest")).unwrap();
        }
        let mut names: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names.len(), KEEP_DAYS as usize, "{names:?}");
        assert_eq!(names.last().unwrap(), "20261124.tsv");
        let back = log.read_since(now - 15 * 3600, now);
        assert_eq!(back.len(), 1);
        assert_eq!(log.read_since(now - 3 * DAY, now).len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }
}
