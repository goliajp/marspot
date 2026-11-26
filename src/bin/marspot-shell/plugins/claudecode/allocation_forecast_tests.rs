use super::*;
use std::fs;

#[test]
fn allocation_forecast_helper_lives_from_the_log() {
    let log = "\
1000\t1000\t7\tstarted\tparent=1\tagent=claude\taccount=P2
1100\t1100\t8\tcarried\tagent=codex\tstarted_ms=500
2000\t61000\t7\tstopped
2100\t2100\t9\tstopped
3000\t30500\t8\tstopped\texited=true
";
    assert_eq!(helper_lives(log), [("claude".to_string(), 60), ("codex".to_string(), 30)]);
}

/// Facts written to the files are folded scan by scan; the forecast is
/// built once an hour and logged when it changed; a shell started again
/// on the same files builds the same line.
#[test]
fn allocation_forecast_scans_and_rebuilds_from_the_files() {
    let root = std::env::temp_dir().join(format!("marspot-forecast-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let dir = events_dir(&root);
    let mut log = EventLog::new(dir.clone());
    let t0 = 1_795_478_400; // 2026-11-25T00:00Z
    let used = |at: i64, sid: u64| Event { at, sid, kind: "used".into(), profile: Some(3), class: "plain".into(), detail: "model=claude-opus-5-5 in=0 cc5m=0 cc1h=0 cr=1000000 out=10000".into() };
    for k in 0..26 {
        log.append(&used(t0 + k * HOUR + 60, 5)).unwrap();
    }
    let b = Event { at: t0 + 25 * HOUR + 90, sid: 5, kind: "boundary".into(), profile: Some(3), class: "plain".into(), detail: "holds=tasks tasks=1 oldest_s=700 helpers=0".into() };
    log.append(&b).unwrap();
    let hours = dir.with_file_name("alloc_hours");
    fs::create_dir_all(&hours).unwrap();
    let rows: String = (0..26).map(|k| format!("{}\thour\tfable=0.0000 other=0.4000 busy_h=1.000 panes=1\n", t0 + k * HOUR)).collect();
    fs::write(hours.join("202611.tsv"), format!("{rows}{}\thour\tfable=- other=- busy_h=- panes=-\n", t0 + 26 * HOUR)).unwrap();
    fs::create_dir_all(root.join("helpers")).unwrap();
    fs::write(root.join("helpers").join("events.log"), "1\t0\t9\tstarted\tagent=claude\n2\t1200000\t9\tstopped\n").unwrap();

    let now = t0 + 26 * HOUR + 10;
    let mut f = Forecasting::new(&root);
    let line = f.scan(&log, &[5], now).expect("the first build is logged");
    assert!(line.contains("other=level:0.40 "), "{line}");
    assert!(line.contains("background_s=700") && line.contains("helpers=claude:1200s/1"), "{line}");
    assert!(line.contains(" 5=0.00/"), "{line}");
    assert_eq!(f.scan(&log, &[5], now + 60), None, "built once an hour");
    // a fact after the start is folded from the tail
    log.append(&used(now + 120, 6)).unwrap();
    f.scan(&log, &[5, 6], now + 130);
    let later = t0 + 27 * HOUR + 5;
    let a = f.scan(&log, &[5, 6], later);
    let mut again = Forecasting::new(&root);
    let b = again.scan(&log, &[5, 6], later);
    assert!(a.is_some());
    assert_eq!(a, b, "rebuilt from the files, the same forecast");
    assert_eq!(allocation_hours::read_rows(&hours).last(), Some(&(t0 + 26 * HOUR, None)));
    fs::remove_dir_all(&root).unwrap();
}
