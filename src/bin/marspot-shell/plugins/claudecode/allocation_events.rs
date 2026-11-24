//! `alloc_events/YYYYMMDD.tsv`: what happened to each pane, as facts.
//!
//! Append-only, one line per change: `at agent sid kind profile class
//! detail`, tab-separated, `at` in unix seconds, `profile` a number or
//! `-`.  One file per UTC day; files older than fifteen days are
//! deleted whole, so the log stays bounded however long the shell
//! runs.  A restart reads the last fifteen hours back to rebuild what
//! it knew.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
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
                remove_old(&path)?;
            }
        }
        Ok(())
    }

    /// The events appended since the last call, oldest first: only the
    /// bytes written since are read.  The first call reads nothing and
    /// starts after the last whole line of the day's file, so a line
    /// half written at that moment is read once it is whole.
    pub(super) fn read_new(&self, tail: &mut Tail, now: i64) -> Vec<Event> {
        let day = ymd(now);
        let file = |d: &str| self.dir.join(format!("{d}.tsv"));
        let mut out = Vec::new();
        let from = match tail.at.take() {
            Some((d, off)) if d == day => off,
            // the rest of the day before, then the new day's from its start
            Some((d, off)) => {
                read_from(&file(&d), off, &mut out);
                0
            }
            None => whole_end(&file(&day)),
        };
        let end = read_from(&file(&day), from, &mut out);
        tail.at = Some((day, end));
        out
    }

    /// Every event at or after `from` up to `now`, oldest first.
    pub(super) fn read_since(&self, from: i64, now: i64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut day = from - from.rem_euclid(DAY);
        while day <= now {
            if let Ok(text) = fs::read_to_string(self.dir.join(format!("{}.tsv", ymd(day)))) {
                // a line still being written is not an event yet, as in `read_from`
                let text = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
                out.extend(text.lines().filter_map(Event::parse).filter(|e| e.at >= from && e.at <= now));
            }
            day += DAY;
        }
        out
    }
}

/// The whole lines of `path` after byte `from` onto `out`; where they end.
/// A line still being written is left for the next read, and a file
/// shorter than `from` was replaced and is read from its start.
fn read_from(path: &Path, from: u64, out: &mut Vec<Event>) -> u64 {
    let len = fs::metadata(path).map_or(0, |m| m.len());
    let from = if from > len { 0 } else { from };
    let Ok(mut f) = fs::File::open(path) else { return from };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.take(len - from).read_to_end(&mut buf).is_err() {
        return from;
    }
    let whole = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    out.extend(String::from_utf8_lossy(&buf[..whole]).lines().filter_map(Event::parse));
    from + whole as u64
}

/// Where the last whole line of `path` ends; 0 without a file.  Read
/// back from the end a block at a time, so the cost does not grow with
/// the day's file.
fn whole_end(path: &Path) -> u64 {
    let Ok(mut f) = fs::File::open(path) else { return 0 };
    let mut end = f.metadata().map_or(0, |m| m.len());
    let mut buf = [0u8; 4096];
    while end > 0 {
        let start = end.saturating_sub(buf.len() as u64);
        let block = &mut buf[..(end - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(block).is_err() {
            return 0;
        }
        if let Some(i) = block.iter().rposition(|b| *b == b'\n') {
            return start + i as u64 + 1;
        }
        end = start;
    }
    0
}

/// Where `read_new` has read to: a day's file and an offset in it.
#[derive(Debug, Default)]
pub(super) struct Tail {
    at: Option<(String, u64)>,
}

/// Remove an old day file.  The main thread and the scan worker each
/// prune the same directory, and a file the other removed first is not
/// an error: the append that pruned goes on.
fn remove_old(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
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

    /// Two writers prune the same directory: a day file the other one
    /// removed first does not fail the append, and the line is kept.
    #[test]
    fn allocation_events_prune_of_a_file_already_gone_keeps_the_line() {
        let dir = std::env::temp_dir().join(format!("marspot-alloc-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let gone = dir.join("20000101.tsv");
        assert!(remove_old(&gone).is_ok(), "already removed by the other writer");
        fs::write(&gone, "").unwrap();
        remove_old(&gone).unwrap();
        assert!(!gone.exists());
        let now = 1_795_530_644;
        let (mut main, mut worker) = (EventLog::new(dir.clone()), EventLog::new(dir.clone()));
        fs::write(dir.join("20000102.tsv"), "").unwrap();
        main.append(&ev(now, "moved")).unwrap();
        worker.append(&ev(now + 1, "rest")).unwrap();
        assert_eq!(main.read_since(now - 10, now + 10).len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `read_new` hands over each line once, from where it began, across
    /// a day's end, and leaves a half-written line for the next read.
    #[test]
    fn allocation_events_read_new_reads_each_line_once() {
        let dir = std::env::temp_dir().join(format!("marspot-alloc-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let now = 1_795_478_400 + DAY - 10; // ten seconds before a day ends
        let mut log = EventLog::new(dir.clone());
        log.append(&ev(now - 5, "before")).unwrap();
        let mut tail = Tail::default();
        assert!(log.read_new(&mut tail, now).is_empty(), "what came before the first read is not new");
        log.append(&ev(now, "moved")).unwrap();
        let kinds = |v: Vec<Event>| v.into_iter().map(|e| e.kind).collect::<Vec<_>>();
        assert_eq!(kinds(log.read_new(&mut tail, now)), ["moved"]);
        assert!(log.read_new(&mut tail, now).is_empty());
        let mut f = fs::OpenOptions::new().append(true).open(dir.join(format!("{}.tsv", ymd(now)))).unwrap();
        f.write_all(b"1\tclaude\t7\tlate\t3\tplain\t").unwrap();
        assert!(log.read_new(&mut tail, now).is_empty(), "half a line");
        f.write_all(b"x\n").unwrap();
        log.append(&ev(now + 20, "next_day")).unwrap();
        assert_eq!(kinds(log.read_new(&mut tail, now + 20)), ["late", "next_day"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A line half written when the first read comes is read once it is
    /// whole; the whole lines before it are not new, however long the
    /// file has grown.
    #[test]
    fn allocation_events_first_read_keeps_a_half_written_line() {
        let dir = std::env::temp_dir().join(format!("marspot-alloc-half-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let now = 1_795_478_400 + 3600;
        let mut log = EventLog::new(dir.clone());
        for i in 0..400 {
            log.append(&ev(now - 400 + i, "before")).unwrap();
        }
        let mut f = fs::OpenOptions::new().append(true).open(dir.join(format!("{}.tsv", ymd(now)))).unwrap();
        f.write_all(b"1\tclaude\t7\tmoved\t3\tplain\twhy=pl").unwrap();
        let mut tail = Tail::default();
        assert!(log.read_new(&mut tail, now).is_empty(), "nothing is new yet");
        assert_eq!(log.read_since(now - 3600, now).len(), 400, "half a line is no event to a recount either");
        f.write_all(b"an\n").unwrap();
        let got = log.read_new(&mut tail, now);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].kind.as_str(), got[0].detail.as_str()), ("moved", "why=plan"));
        assert!(whole_end(&dir.join("none.tsv")) == 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
