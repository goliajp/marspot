//! A fact log: one file per UTC day, `YYYYMMDD.tsv`, lines in the format
//! of `marspot_term::wire_files::fact_line`, only ever appended to.
//!
//! A day holds at most `cap` bytes of facts.  Past it lines are counted
//! instead of written, and the count goes into one `dropped` line when
//! the day ends or the log is closed, so a reader can tell a quiet day
//! from a full one.  The count of a process that dies first is lost.
//! Days older than `keep_days` are deleted when a new day starts.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use marspot_term::wire_files::fact_line;

const DAY: u64 = 86_400;

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
    bytes: u64,
    dropped: u64,
    last_unix: u64,
}

impl FactLog {
    pub(crate) fn at(dir: PathBuf, cap: u64, keep_days: u32) -> io::Result<FactLog> {
        super::assert_off_main("a fact log opened");
        std::fs::create_dir_all(&dir)?;
        Ok(FactLog { dir, cap, keep_days, day: None, line: String::new() })
    }

    /// Append one fact; false when the day's cap dropped it.
    pub(crate) fn append(&mut self, unix: u64, kind: &str, fields: &[(&str, &str)]) -> io::Result<bool> {
        super::assert_off_main("a fact appended");
        let name = fact_line::day_file(unix);
        if self.day.as_ref().is_none_or(|d| d.name != name) {
            self.close_day()?;
            self.day = Some(open_day(&self.dir, name)?);
            prune(&self.dir, unix, self.keep_days)?;
        }
        self.line.clear();
        fact_line::push(&mut self.line, unix, kind, fields);
        let day = self.day.as_mut().expect("opened above");
        day.last_unix = unix;
        if day.bytes + self.line.len() as u64 > self.cap {
            day.dropped += 1;
            return Ok(false);
        }
        day.file.write_all(self.line.as_bytes())?;
        day.bytes += self.line.len() as u64;
        Ok(true)
    }

    /// Lines the cap dropped today, not yet counted in the file.
    pub(crate) fn dropped(&self) -> u64 {
        self.day.as_ref().map_or(0, |d| d.dropped)
    }

    /// Write the day's `dropped` line, if lines were dropped; it is the
    /// one line allowed past the cap.
    fn close_day(&mut self) -> io::Result<()> {
        let Some(mut day) = self.day.take() else { return Ok(()) };
        if day.dropped == 0 {
            return Ok(());
        }
        self.line.clear();
        fact_line::push_dropped(&mut self.line, day.last_unix, day.dropped);
        day.file.write_all(self.line.as_bytes())
    }
}

impl Drop for FactLog {
    fn drop(&mut self) {
        if let Err(e) = self.close_day() {
            marspot::lx_warn!("plat.fact_log.dropped_unwritten", &format!("{}: {e}", self.dir.display()));
        }
    }
}

/// Open a day's file to append, after whatever an earlier process wrote
/// there.  A last line cut short is ended first, so the next fact is
/// never glued onto it.
fn open_day(dir: &Path, name: String) -> io::Result<Day> {
    let mut file = OpenOptions::new().create(true).read(true).append(true).open(dir.join(&name))?;
    let mut bytes = file.metadata()?.len();
    if bytes > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            file.write_all(b"\n")?;
            bytes += 1;
        }
    }
    Ok(Day { name, file, bytes, dropped: 0, last_unix: 0 })
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
