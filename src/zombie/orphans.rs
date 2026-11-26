//! The record of processes that closed panes left running,
//! `<state>/zombie/orphans.tsv`: the shell's zombie watch writes it,
//! the process monitor reads it to list them.  One line per process:
//!
//! `first_seen_unix  pid@start_us  sid  name  rules  argv`
//!
//! `name` is the pane's name before it closed (empty when it was never
//! seen), `rules` the outside rules it meets (`O1,O2`, or `-`), `argv`
//! its first [`ARGV_BYTES`] bytes.  A reader skips lines it cannot read
//! and columns past the ones it knows, so a later build may add some.

use std::path::{Path, PathBuf};

use crate::pidtree::ProcId;

/// The most lines the record holds; the newest are kept.
pub const MAX_ROWS: usize = 512;
/// How much of a command line is kept.
pub const ARGV_BYTES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    /// When the watch first saw it, unix seconds.
    pub first_seen: u64,
    pub id: ProcId,
    /// The closed pane it belongs to.
    pub sid: u64,
    pub name: String,
    /// `O1`, `O2`, both comma-separated, or empty.
    pub rules: String,
    pub argv: String,
}

pub fn path(state_root: &Path) -> PathBuf {
    state_root.join("zombie").join("orphans.tsv")
}

/// `s` with no tab or line break, at most `max` bytes, cut on a
/// character boundary.
pub fn field(s: &str, max: usize) -> String {
    let mut out: String = s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect();
    if out.len() > max {
        let mut end = max;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

pub fn parse_id(s: &str) -> Option<ProcId> {
    let (pid, start) = s.split_once('@')?;
    Some(ProcId { pid: pid.parse().ok()?, start_us: start.parse().ok()? })
}

pub fn parse(body: &str) -> Vec<Orphan> {
    body.lines().filter_map(line).collect()
}

fn line(l: &str) -> Option<Orphan> {
    let mut f = l.split('\t');
    let first_seen = f.next()?.parse().ok()?;
    let id = parse_id(f.next()?)?;
    let sid = f.next()?.parse().ok()?;
    let name = f.next()?.to_string();
    let rules = f.next()?;
    let argv = f.next().unwrap_or("").to_string();
    let rules = if rules == "-" { String::new() } else { rules.to_string() };
    Some(Orphan { first_seen, id, sid, name, rules, argv })
}

/// The record's text for `rows`, at most [`MAX_ROWS`] of them, the
/// newest kept.
pub fn format(rows: &[Orphan]) -> String {
    let mut sorted: Vec<&Orphan> = rows.iter().collect();
    sorted.sort_by_key(|o| (o.first_seen, o.id.pid, o.id.start_us));
    let skip = sorted.len().saturating_sub(MAX_ROWS);
    let mut out = String::new();
    for o in &sorted[skip..] {
        let rules = if o.rules.is_empty() { "-" } else { o.rules.as_str() };
        out.push_str(&format!(
            "{}\t{}@{}\t{}\t{}\t{}\t{}\n",
            o.first_seen,
            o.id.pid,
            o.id.start_us,
            o.sid,
            field(&o.name, 128),
            field(rules, 16),
            field(&o.argv, ARGV_BYTES)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orphan(first_seen: u64, pid: i32) -> Orphan {
        Orphan {
            first_seen,
            id: ProcId { pid, start_us: 1_700_000_000_000_000 + pid as u64 },
            sid: 493,
            name: "spg".into(),
            rules: String::new(),
            argv: "codex app-server".into(),
        }
    }

    #[test]
    fn zombie_orphans_record_reads_back_what_it_wrote() {
        let mut a = orphan(10, 4242);
        a.rules = "O1,O2".into();
        a.argv = "tail\t-f\nlog".into();
        let rows = vec![a.clone(), orphan(5, 77)];
        let back = parse(&format(&rows));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], orphan(5, 77), "oldest first");
        assert_eq!(back[1].rules, "O1,O2");
        assert_eq!(back[1].argv, "tail -f log", "no tab or line break survives in a field");
    }

    #[test]
    fn zombie_orphans_record_skips_what_it_cannot_read_and_columns_it_does_not_know() {
        let body = "junk\n1\t2@3\t4\tname\t-\targv\textra\n5\tnot-an-id\t6\tx\t-\ty\n";
        let rows = parse(body);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].id.pid, rows[0].id.start_us, rows[0].sid), (2, 3, 4));
        assert_eq!((rows[0].rules.as_str(), rows[0].argv.as_str()), ("", "argv"));
    }

    #[test]
    fn zombie_orphans_record_keeps_the_newest_rows_and_cuts_long_fields() {
        let rows: Vec<Orphan> = (0..MAX_ROWS as u64 + 10).map(|i| orphan(i, i as i32 + 2)).collect();
        let back = parse(&format(&rows));
        assert_eq!(back.len(), MAX_ROWS);
        assert_eq!(back[0].first_seen, 10);
        assert_eq!(field(&"é".repeat(150), ARGV_BYTES).len(), ARGV_BYTES);
        assert_eq!(field(&format!("a{}", "é".repeat(150)), ARGV_BYTES).len(), ARGV_BYTES - 1, "on a char boundary");
    }
}
