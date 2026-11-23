use super::super::audit::{MERGE_MS, Merger, REJECT_EVERY_MS, Rec};
use super::super::audit_log::{self, Audit, KEEP_FILES, rotate, utc_text};
use super::temp;

fn utc(ms: u64) -> String {
    format!("t{ms}")
}

fn rec(at_ms: u64, caller: &str, sid: u64, text: &str, echo_off: bool) -> Rec {
    Rec {
        at_ms,
        caller: caller.into(),
        pid: Some(1),
        exe: "/bin/x".into(),
        op: "pane.send",
        pane: format!("p#{sid}"),
        sid,
        content: Some(text.as_bytes().to_vec()),
        echo_off,
        result: "written".into(),
    }
}

#[test]
fn one_callers_inputs_within_a_second_are_one_line() {
    let mut m = Merger::default();
    let mut lines = Vec::new();
    for i in 0..20 {
        lines.extend(m.add(rec(1_000 + i * 40, "a#1", 5, "ls", false), &utc));
    }
    // another caller and another pane are lines of their own
    lines.extend(m.add(rec(1_500, "b#2", 5, "x", false), &utc));
    lines.extend(m.add(rec(1_500, "a#1", 6, "y", false), &utc));
    assert!(lines.is_empty(), "nothing is written while a group is open: {lines:?}");
    // the two single records are due first, the run of 20 a second after its last
    assert_eq!(m.due(), Some(1_500 + MERGE_MS));
    assert_eq!(m.flush_due(1_500 + MERGE_MS, &utc).len(), 2);
    assert_eq!(m.due(), Some(1_000 + 19 * 40 + MERGE_MS));
    let out = m.flush_due(1_000 + 19 * 40 + MERGE_MS, &utc);
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("\"count\":20") && out[0].contains("\"bytes\":40") && out[0].contains("\"at\":\"t1000\""), "{}", out[0]);
    assert!(m.flush_all(&utc).is_empty());
    // a pause of more than a second starts a new line
    let mut m = Merger::default();
    m.add(rec(0, "a#1", 5, "one", false), &utc);
    let out = m.add(rec(MERGE_MS + 1, "a#1", 5, "two", false), &utc);
    assert_eq!(out.len(), 1);
}

#[test]
fn refusals_are_a_line_per_ten_seconds() {
    let mut m = Merger::default();
    let first = m.refused("a#1", "limit_rate", "pane.send", "p", 0, &utc);
    assert_eq!(first.len(), 1);
    for t in 1..100 {
        assert!(m.refused("a#1", "limit_rate", "pane.send", "p", t * 50, &utc).is_empty());
    }
    let summary = m.flush_due(REJECT_EVERY_MS, &utc);
    assert_eq!(summary.len(), 1);
    assert!(summary[0].contains("\"count\":99"), "{}", summary[0]);
    // another error word is a line of its own
    assert_eq!(m.refused("a#1", "pane_busy", "pane.send", "p", REJECT_EVERY_MS, &utc).len(), 1);
}

#[test]
fn echo_off_keeps_only_length_and_hash() {
    let mut m = Merger::default();
    m.add(rec(0, "a#1", 5, "hunter2", true), &utc);
    let out = m.flush_all(&utc);
    assert!(!out[0].contains("hunter2") && !out[0].contains("\"text\""), "{}", out[0]);
    assert!(out[0].contains("\"sha256_16\"") && out[0].contains("\"bytes\":7") && out[0].contains("\"echo_off\":true"));
    let mut m = Merger::default();
    m.add(rec(0, "a#1", 5, "echo \u{1b}[31mhi", false), &utc);
    assert!(m.flush_all(&utc)[0].contains("\"text\":\"echo [31mhi\""));
}

#[test]
fn rotation_keeps_eight_files() {
    let dir = temp("rot");
    let log = dir.join("a.log");
    for i in 0..12 {
        std::fs::write(&log, format!("{i}\n")).unwrap();
        rotate(&log);
    }
    let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names.len(), KEEP_FILES - 1, "{names:?}");
    std::fs::write(&log, "live\n").unwrap();
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), KEEP_FILES);
    assert_eq!(std::fs::read_to_string(dir.join("a.log.1")).unwrap(), "11\n");
    assert_eq!(std::fs::read_to_string(dir.join("a.log.7")).unwrap(), "5\n");
}

#[test]
fn the_writer_rotates_and_writes_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp("w");
    let log = dir.join("audit.log");
    let a = Audit::at(log.clone(), 600);
    for i in 0..20 {
        a.record(rec(i * 2_000, &format!("c{i}"), i, "some text typed", false));
        a.refused(format!("c{i}"), "limit_rate", "pane.send", "p".into());
    }
    a.flush();
    assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(dir.join("audit.log.1").exists());
    // the file and the one before it hold the latest lines
    assert_eq!(audit_log::tail(&log, 2).len(), 2);
}

#[test]
fn utc_text_is_calendar_time() {
    assert_eq!(utc_text(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(utc_text(1_795_000_000_123), "2026-11-18T11:06:40.123Z");
    assert_eq!(utc_text(951_782_400_000), "2000-02-29T00:00:00.000Z");
}
