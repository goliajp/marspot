use super::*;
use marspot_term::test_env::TempDir;
use marspot_term::wire_files::fact_line::{Line, dropped_count};

// 2025-11-26 00:00:00 UTC
const D0: u64 = 1_764_115_200;

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap().lines().map(String::from).collect()
}

/// What a reader makes of a day: its facts and the sum of its dropped
/// lines.
fn read_day(path: &Path) -> (Vec<String>, u64) {
    let all = lines(path);
    let dropped = all.iter().filter_map(|l| dropped_count(l)).sum();
    (all.into_iter().filter(|l| dropped_count(l).is_none()).collect(), dropped)
}

#[test]
fn facts_go_to_the_file_of_their_utc_day() {
    let tmp = TempDir::new("fact-log-days");
    let mut log = FactLog::at(tmp.join("events"), 1 << 20, 15).unwrap();
    assert!(log.append(D0 + 5, "refused", &[("agent", "claude"), ("home", "2")]).unwrap());
    assert!(log.append(D0 + DAY - 1, "moved", &[("to", "3")]).unwrap());
    assert!(log.append(D0 + DAY, "moved", &[("to", "1")]).unwrap());
    let first = lines(&tmp.join("events/20251126.tsv"));
    assert_eq!(first, ["1764115205\trefused\tagent=claude\thome=2", "1764201599\tmoved\tto=3"]);
    let l = Line::parse(&first[0]).unwrap();
    assert_eq!(l.get("home").as_deref(), Some("2"));
    assert_eq!(lines(&tmp.join("events/20251127.tsv")), ["1764201600\tmoved\tto=1"]);
}

#[test]
fn past_the_cap_facts_are_counted_on_disk_at_every_drop() {
    let tmp = TempDir::new("fact-log-cap");
    let one = "1764115200\tk\tv=x\n".len() as u64;
    let mut log = FactLog::at(tmp.path().to_path_buf(), 2 * one, 15).unwrap();
    assert!(log.append(D0, "k", &[("v", "x")]).unwrap());
    assert!(log.append(D0, "k", &[("v", "x")]).unwrap());
    assert!(!log.append(D0 + 1, "k", &[("v", "x")]).unwrap());
    assert_eq!(read_day(&tmp.join("20251126.tsv")).1, 1, "the first drop is on disk at once");
    assert!(!log.append(D0 + 2, "k", &[("v", "x")]).unwrap());
    assert_eq!(log.dropped(), 2);
    let day = lines(&tmp.join("20251126.tsv"));
    assert_eq!(day.len(), 3);
    assert_eq!(day[2], "1764115201\tdropped\tdropped=00000000000000000002");
    assert!(log.append(D0 + DAY, "k", &[("v", "x")]).unwrap());
    assert_eq!(log.dropped(), 0);
}

#[test]
fn a_process_that_execs_without_closing_loses_no_count() {
    let tmp = TempDir::new("fact-log-exec");
    let mut log = FactLog::at(tmp.path().to_path_buf(), 1, 15).unwrap();
    for i in 0..3 {
        assert!(!log.append(D0 + i, "k", &[]).unwrap());
    }
    // exec runs no destructors; it closes the close-on-exec descriptor,
    // and with it the lock
    let fd = log.day.as_ref().unwrap().file.as_raw_fd();
    // SAFETY: fcntl and close on a descriptor the forgotten log owned
    assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0, "the day goes away at exec");
    std::mem::forget(log);
    assert_eq!(unsafe { libc::close(fd) }, 0);
    let mut next = FactLog::at(tmp.path().to_path_buf(), 1, 15).unwrap();
    assert!(!next.append(D0 + 9, "k", &[]).unwrap());
    assert_eq!(next.dropped(), 4);
    drop(next);
    assert_eq!(read_day(&tmp.join("20251126.tsv")), (vec![], 4));
    assert_eq!(lines(&tmp.join("20251126.tsv")).len(), 1, "one dropped line, raised in place");
}

#[test]
fn a_second_writer_on_the_same_day_takes_no_lock_and_writes_nothing() {
    let tmp = TempDir::new("fact-log-two-writers");
    let mut first = FactLog::at(tmp.path().to_path_buf(), 1 << 20, 15).unwrap();
    assert!(first.append(D0, "k", &[("v", "1")]).unwrap());
    // a torn tail the second writer must not cut while the first holds the day
    first.day.as_ref().unwrap().file.write_all_at(b"1764115201\tk\tv=", first.day.as_ref().unwrap().len).unwrap();
    let before = std::fs::read(tmp.join("20251126.tsv")).unwrap();
    let mut second = FactLog::at(tmp.path().to_path_buf(), 1 << 20, 15).unwrap();
    let err = second.append(D0 + 1, "k", &[("v", "2")]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock, "{err}");
    assert_eq!(std::fs::read(tmp.join("20251126.tsv")).unwrap(), before, "the refused writer touched nothing");
    drop(first);
    assert!(second.append(D0 + 2, "k", &[("v", "3")]).unwrap(), "the lock is tried again at the next append");
    assert_eq!(lines(&tmp.join("20251126.tsv")), ["1764115200\tk\tv=1", "1764115202\tk\tv=3"]);
}

#[test]
fn a_reopened_day_counts_what_is_there_and_cuts_off_a_torn_line() {
    let tmp = TempDir::new("fact-log-reopen");
    std::fs::write(tmp.join("20251126.tsv"), "1764115200\tk\tv=1\n1764115201\tmoved\tto=1").unwrap();
    let whole = "1764115200\tk\tv=1\n".len() as u64;
    let mut log = FactLog::at(tmp.path().to_path_buf(), whole + 20, 15).unwrap();
    assert!(log.append(D0 + 2, "k", &[("v", "3")]).unwrap());
    assert!(!log.append(D0 + 3, "k", &[("v", "4")]).unwrap(), "the earlier process's bytes count against the cap");
    let (facts, dropped) = read_day(&tmp.join("20251126.tsv"));
    assert_eq!(facts, ["1764115200\tk\tv=1", "1764115202\tk\tv=3"], "the torn `to=1` never reads as a fact");
    assert_eq!(dropped, 1);
}

#[test]
fn a_day_of_only_a_torn_line_starts_empty() {
    let tmp = TempDir::new("fact-log-torn-only");
    std::fs::write(tmp.join("20251126.tsv"), "1764115201\tmov").unwrap();
    let mut log = FactLog::at(tmp.path().to_path_buf(), 1 << 20, 15).unwrap();
    assert!(log.append(D0 + 2, "k", &[]).unwrap());
    assert_eq!(lines(&tmp.join("20251126.tsv")), ["1764115202\tk"]);
}

#[test]
fn a_new_day_deletes_the_days_past_keeping() {
    let tmp = TempDir::new("fact-log-prune");
    for d in ["20251120.tsv", "20251124.tsv", "20251125.tsv", "notes.tsv"] {
        std::fs::write(tmp.join(d), "").unwrap();
    }
    let mut log = FactLog::at(tmp.path().to_path_buf(), 1 << 20, 2).unwrap();
    log.append(D0, "k", &[]).unwrap();
    let mut left: Vec<String> =
        std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["20251125.tsv", "20251126.tsv", "notes.tsv"]);
}
