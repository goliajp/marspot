//! A fact log: one file per UTC day, `YYYYMMDD.tsv`, lines in the format
//! of `marspot_term::wire_files::fact_line`, only ever appended to.
//!
//! A day holds at most `cap` bytes of facts.  Past it a fact is counted
//! instead of written: the first one adds a `dropped` line, and each
//! later one raises the count in that line in place.  The count is on
//! disk at every drop, so a process that execs or dies loses none of
//! it, and the next process to open the day goes on raising the same
//! line.  A reader can tell a quiet day from a full one.
//!
//! One writer per day file: a log takes an exclusive `flock` on the day
//! before it reads or changes it, and holds it while the day is open.
//! Lines go at the offset this writer knows, so a second writer would
//! overwrite the first's lines and could point the dropped count into
//! them.  A second writer (another log on the same directory in this
//! process, or another marspot-shell process such as a command-line
//! mode) does not wait: its append fails with `WouldBlock`, writes
//! nothing, and the caller decides what the fact is worth.  Its next
//! append asks for the lock again and fails the same way for as long as
//! the holder keeps the day open, which for the running L1 is the whole
//! day: a command-line marspot-shell cannot write a day L1 is writing.
//! The descriptor is close-on-exec, so an exec releases the lock and the
//! new image takes it at its first append.  A child forked just before
//! (the launch of core) shares the locked description until its own
//! exec, so the new image's first append in that moment can fail with
//! `WouldBlock` too.
//!
//! A last line an earlier process left cut short is cut off on open: it
//! was never a whole fact.  Days older than `keep_days` are deleted when
//! a new day starts.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use marspot_term::wire_files::fact_line;

const DAY: u64 = 86_400;

/// How much of a day's tail is read at a time looking for its last line.
const TAIL_CHUNK: u64 = 4096;

pub(crate) struct FactLog {
    dir: PathBuf,
    cap: u64,
    keep_days: u32,
    day: Option<Day>,
    line: String,
}

struct Day {
    name: String,
    file: File,
    /// The file's length: where the next line goes.
    len: u64,
    dropped: u64,
    /// Where the digits of today's dropped line are, once there is one.
    count_at: Option<u64>,
}

impl FactLog {
    pub(crate) fn at(dir: PathBuf, cap: u64, keep_days: u32) -> io::Result<FactLog> {
        super::assert_off_main("a fact log opened");
        std::fs::create_dir_all(&dir)?;
        Ok(FactLog { dir, cap, keep_days, day: None, line: String::new() })
    }

    /// Append one fact; false when the day's cap dropped it.  An error
    /// of kind `WouldBlock` when another writer holds the day: nothing
    /// was written.
    pub(crate) fn append(&mut self, unix: u64, kind: &str, fields: &[(&str, &str)]) -> io::Result<bool> {
        super::assert_off_main("a fact appended");
        let name = fact_line::day_file(unix);
        if self.day.as_ref().is_none_or(|d| d.name != name) {
            self.day = Some(open_day(&self.dir, name)?);
            prune(&self.dir, unix, self.keep_days)?;
        }
        self.line.clear();
        fact_line::push(&mut self.line, unix, kind, fields);
        let day = self.day.as_mut().expect("opened above");
        if day.count_at.is_none() && day.len + self.line.len() as u64 <= self.cap {
            day.file.write_all_at(self.line.as_bytes(), day.len)?;
            day.len += self.line.len() as u64;
            return Ok(true);
        }
        day.dropped += 1;
        match day.count_at {
            Some(at) => day.file.write_all_at(format!("{:0w$}", day.dropped, w = fact_line::DROPPED_WIDTH).as_bytes(), at)?,
            None => {
                // the one line allowed past the cap
                self.line.clear();
                fact_line::push_dropped(&mut self.line, unix, day.dropped);
                day.file.write_all_at(self.line.as_bytes(), day.len)?;
                day.len += self.line.len() as u64;
                day.count_at = Some(day.len - 1 - fact_line::DROPPED_WIDTH as u64);
            }
        }
        Ok(false)
    }

    /// Facts the cap dropped today, by every process that wrote the day.
    pub(crate) fn dropped(&self) -> u64 {
        self.day.as_ref().map_or(0, |d| d.dropped)
    }
}

/// Open a day's file to append after whatever an earlier process wrote
/// there, cutting off a last line it left unfinished, and pick up the
/// count of a dropped line that ends the file.
fn open_day(dir: &Path, name: String) -> io::Result<Day> {
    let path = dir.join(&name);
    let mut file = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path)?;
    // SAFETY: flock only takes a lock on a descriptor this function owns
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("another writer holds {}", path.display())));
        }
        return Err(e);
    }
    let size = file.metadata()?.len();
    let (whole, last) = last_line(&mut file, size)?;
    if whole < size {
        file.set_len(whole)?;
    }
    let mut day = Day { name, file, len: whole, dropped: 0, count_at: None };
    if let Some(n) = last.as_deref().and_then(fact_line::dropped_count) {
        day.dropped = n;
        day.count_at = Some(whole - 1 - fact_line::DROPPED_WIDTH as u64);
    }
    Ok(day)
}

/// The length of `file` up to and including its last newline, and the
/// last whole line (newline included) when there is one.
fn last_line(file: &mut File, size: u64) -> io::Result<(u64, Option<String>)> {
    let mut tail: Vec<u8> = Vec::new();
    let mut from = size;
    let mut newlines = Vec::new();
    while from > 0 && newlines.len() < 2 {
        let start = from.saturating_sub(TAIL_CHUNK);
        let mut chunk = vec![0u8; (from - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
        from = start;
        newlines = tail.iter().enumerate().rev().filter(|(_, b)| **b == b'\n').map(|(i, _)| i).take(2).collect();
    }
    let Some(&end) = newlines.first() else { return Ok((0, None)) };
    let begin = newlines.get(1).map_or(0, |i| i + 1);
    let whole = from + end as u64 + 1;
    Ok((whole, String::from_utf8(tail[begin..=end].to_vec()).ok()))
}

/// Delete the day files of `dir` from before the last `keep_days` days.
fn prune(dir: &Path, now: u64, keep_days: u32) -> io::Result<()> {
    let oldest = fact_line::day(now.saturating_sub(u64::from(keep_days.saturating_sub(1)) * DAY));
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(day) = name.to_str().and_then(fact_line::day_of_file) else { continue };
        if day < oldest.as_str() {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
