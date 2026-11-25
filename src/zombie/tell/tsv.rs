//! The pacing as two tab-separated files, so a new shell image goes on
//! from where the old one was: `told.tsv` (`sid  id  first_told_unix`)
//! and `clock.tsv` (`pane  <sid>  unix` for each message to a pane and
//! `host  -  unix` for each message at all, within the last day).
//!
//! A newer image may add columns; a reader takes the ones it knows and
//! skips the rest, and skips a row it cannot read.  Each file is read
//! up to [`ROWS`] rows.

use super::{DAY, Pacer, TOLD_CAP};

pub const ROWS: usize = TOLD_CAP;

impl Pacer {
    pub fn told_tsv(&self) -> String {
        let mut rows: Vec<(&String, &(u64, u64))> = self.told.iter().collect();
        rows.sort_by_key(|(id, (sid, at))| (*at, *sid, (*id).clone()));
        let mut out = String::from("# sid\tid\tfirst_told_unix\n");
        for (id, (sid, at)) in rows {
            out.push_str(&format!("{sid}\t{id}\t{at}\n"));
        }
        out
    }

    pub fn clock_tsv(&self) -> String {
        let mut out = String::from("# kind\tsid\tunix\n");
        let mut panes: Vec<_> = self.panes.iter().collect();
        panes.sort_by_key(|(sid, _)| **sid);
        for (sid, q) in panes {
            for at in q {
                out.push_str(&format!("pane\t{sid}\t{at}\n"));
            }
        }
        for at in &self.host {
            out.push_str(&format!("host\t-\t{at}\n"));
        }
        out
    }

    /// The pacing the two files hold, as of `now`: told times older
    /// than a day are dropped.
    pub fn from_tsv(told: &str, clock: &str, now: u64) -> Pacer {
        let mut p = Pacer::default();
        for c in rows(told) {
            let (Some(sid), Some(id), Some(at)) = (num(c.first()), c.get(1), num(c.get(2))) else { continue };
            if !id.is_empty() {
                p.told.entry(id.to_string()).or_insert((sid, at));
            }
        }
        p.cap_told();
        let mut times: Vec<(Option<u64>, u64)> = Vec::new();
        for c in rows(clock) {
            let Some(at) = num(c.get(2)).filter(|at| now < at + DAY) else { continue };
            match c.first().copied() {
                Some("pane") => {
                    if let Some(sid) = num(c.get(1)) {
                        times.push((Some(sid), at));
                    }
                }
                Some("host") => times.push((None, at)),
                _ => {}
            }
        }
        times.sort_by_key(|(_, at)| *at);
        for (sid, at) in times {
            match sid {
                Some(sid) => p.panes.entry(sid).or_default().push_back(at),
                None => p.host.push_back(at),
            }
        }
        p
    }
}

fn rows(body: &str) -> impl Iterator<Item = Vec<&str>> {
    body.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).take(ROWS).map(|l| l.split('\t').collect())
}

fn num(s: Option<&&str>) -> Option<u64> {
    s?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::super::{HOUR, PANE_GAP};
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn zombie_pacing_survives_a_round_trip_through_its_files() {
        let mut p = Pacer::default();
        p.record(7, NOW - 2 * DAY, ["1@2".to_string()]);
        p.record(7, NOW - HOUR, ["agent:a1".to_string(), "3@4".to_string()]);
        p.record(9, NOW - 60, []);
        let q = Pacer::from_tsv(&p.told_tsv(), &p.clock_tsv(), NOW);
        assert!(q.told("1@2") && q.told("agent:a1") && q.told("3@4"));
        assert_eq!(q.told["agent:a1"], (7, NOW - HOUR));
        assert!(!q.allows(7, NOW + PANE_GAP - HOUR - 1), "pane 7 was told an hour ago");
        assert!(q.allows(7, NOW + PANE_GAP - HOUR));
        assert_eq!(q.host.len(), 2, "the message of two days ago no longer counts");
    }

    #[test]
    fn zombie_pacing_files_skip_unknown_columns_and_rows_and_are_bounded() {
        let told = "# sid\tid\tfirst_told_unix\textra\n7\t1@2\t100\tlater\nnot a row\n8\t\t5\n";
        let clock = "pane\t7\t1799999000\tx\nhost\t-\t1799999000\nweird\t1\t2\n";
        let p = Pacer::from_tsv(told, clock, NOW);
        assert_eq!(p.told.len(), 1);
        assert_eq!(p.told["1@2"], (7, 100));
        assert!(!p.allows(7, NOW));
        let many: String = (0..ROWS + 100).map(|i| format!("1\t{i}@1\t{i}\n")).collect();
        assert_eq!(Pacer::from_tsv(&many, "", NOW).told.len(), ROWS);
    }
}
