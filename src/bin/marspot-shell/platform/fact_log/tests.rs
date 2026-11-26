use super::*;
use marspot_term::test_env::TempDir;
use marspot_term::wire_files::fact_line::Line;

// 2025-11-26 00:00:00 UTC
const D0: u64 = 1_764_115_200;

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap().lines().map(String::from).collect()
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
fn past_the_cap_lines_are_counted_and_the_count_written_when_the_day_ends() {
    let tmp = TempDir::new("fact-log-cap");
    let one = "1764115200\tk\tv=x\n".len() as u64;
    let mut log = FactLog::at(tmp.path().to_path_buf(), 2 * one, 15).unwrap();
    assert!(log.append(D0, "k", &[("v", "x")]).unwrap());
    assert!(log.append(D0, "k", &[("v", "x")]).unwrap());
    assert!(!log.append(D0 + 1, "k", &[("v", "x")]).unwrap());
    assert!(!log.append(D0 + 2, "k", &[("v", "x")]).unwrap());
    assert_eq!(log.dropped(), 2);
    assert_eq!(lines(&tmp.join("20251126.tsv")).len(), 2);
    assert!(log.append(D0 + DAY, "k", &[("v", "x")]).unwrap());
    assert_eq!(log.dropped(), 0);
    let day = lines(&tmp.join("20251126.tsv"));
    assert_eq!(day.last().map(String::as_str), Some("1764115202\tdropped\tdropped=2"));
}

#[test]
fn closing_the_log_writes_the_count_of_the_day_so_far() {
    let tmp = TempDir::new("fact-log-close");
    let mut log = FactLog::at(tmp.path().to_path_buf(), 1, 15).unwrap();
    assert!(!log.append(D0 + 9, "k", &[]).unwrap());
    drop(log);
    assert_eq!(lines(&tmp.join("20251126.tsv")), ["1764115209\tdropped\tdropped=1"]);
}

#[test]
fn a_reopened_day_counts_what_is_there_and_ends_a_cut_line() {
    let tmp = TempDir::new("fact-log-reopen");
    std::fs::write(tmp.join("20251126.tsv"), "1764115200\tk\tv=1\n1764115201\tk\tv=").unwrap();
    let have = std::fs::metadata(tmp.join("20251126.tsv")).unwrap().len();
    let mut log = FactLog::at(tmp.path().to_path_buf(), have + 1 + 20, 15).unwrap();
    assert!(log.append(D0 + 2, "k", &[("v", "3")]).unwrap());
    assert!(!log.append(D0 + 3, "k", &[("v", "4")]).unwrap(), "the earlier process's bytes count against the cap");
    let day = lines(&tmp.join("20251126.tsv"));
    assert_eq!(day[1], "1764115201\tk\tv=");
    assert_eq!(day[2], "1764115202\tk\tv=3");
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
