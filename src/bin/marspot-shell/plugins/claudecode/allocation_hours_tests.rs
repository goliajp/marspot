use super::*;

/// 2026-11-26T18:00Z.
const H18: i64 = 1_795_716_000;

fn ev(at: i64, sid: u64, kind: &str, class: &str, detail: &str) -> Event {
    Event { at, sid, kind: kind.into(), profile: Some(3), class: class.into(), detail: detail.into() }
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-hours-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

#[test]
fn allocation_hours_sum_both_speeds_and_the_busy_pane_hours_of_each_hour() {
    // 1.12 dollars of Fable output is 0.1 point; 3.05 dollars of Opus output is 0.1 point
    let fable = "model=claude-fable-5-2 in=0 cc5m=0 cc1h=0 cr=0 out=28000";
    let opus = "model=claude-opus-5-5 in=0 cc5m=0 cc1h=0 cr=0 out=152500";
    let facts = [
        ev(H18 - 1800, 481, "busy", "plain", ""),
        ev(H18 + 600, 481, "used", "plain", opus),
        ev(H18 + 900, 481, "rest", "plain", ""),
        ev(H18 + 1200, 428, "busy", "fable", ""),
        ev(H18 + 1300, 428, "used", "fable", fable),
        ev(H18 + 1800, 428, "bound", "fable", "from P3"),
        ev(H18 + 3700, 428, "used", "fable", fable),
        Event { profile: None, ..ev(H18 + 5400, 428, "bound", "plain", "pane gone") },
    ];
    let rows = sum(&facts, H18, H18 + HOUR);
    assert_eq!(rows.len(), 2);
    let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(near(rows[0].fable, 0.1) && near(rows[0].other, 0.1), "{rows:?}");
    assert!(near(rows[0].busy_h, 0.25 + 2400.0 / 3600.0), "481 until it rests, 428 through its move: {rows:?}");
    assert_eq!(rows[0].panes, 2);
    assert!(near(rows[1].fable, 0.1) && near(rows[1].other, 0.0) && near(rows[1].busy_h, 0.5), "{rows:?}");
}

#[test]
fn allocation_hours_append_each_hour_once_catch_up_after_a_gap_and_keep_sixty_days() {
    let root = dir("tick");
    let events = EventLog::new(root.join("events"));
    let hours_dir = root.join("hours");
    let mut log = EventLog::new(root.join("events"));
    log.append(&ev(H18 + 60, 1, "busy", "plain", "")).unwrap();
    let mut h = Hours::new(hours_dir.clone());
    assert_eq!(h.tick(&events, H18 + HOUR + 5).unwrap(), 1, "18:00 ended");
    assert_eq!(h.tick(&events, H18 + HOUR + 900).unwrap(), 0, "once");
    // the shell was down for three hours; a new one finds where the file ends
    let mut h = Hours::new(hours_dir.clone());
    assert_eq!(h.tick(&events, H18 + 4 * HOUR + 1).unwrap(), 3);
    let text = fs::read_to_string(hours_dir.join("202611.tsv")).unwrap();
    let starts: Vec<i64> = text.lines().map(|l| l.split('\t').next().unwrap().parse().unwrap()).collect();
    assert_eq!(starts, [H18, H18 + HOUR, H18 + 2 * HOUR, H18 + 3 * HOUR]);
    assert!(text.lines().next().unwrap().ends_with("\thour\tfable=0.0000 other=0.0000 busy_h=0.983 panes=1"), "{text}");
    fs::write(hours_dir.join("202608.tsv"), "").unwrap();
    fs::write(hours_dir.join("202609.tsv"), "").unwrap();
    let mut h = Hours::new(hours_dir.clone());
    // 2026-11-30: sixty days back is 2026-10-01, so September goes
    let late = H18 + 4 * DAY;
    h.tick(&events, late).unwrap();
    assert!(!hours_dir.join("202608.tsv").exists() && !hours_dir.join("202609.tsv").exists());
    let text = fs::read_to_string(hours_dir.join("202611.tsv")).unwrap();
    assert_eq!(text.lines().count(), 4 + CATCH_UP as usize, "a long gap is summed back two days at most");
    fs::remove_dir_all(&root).unwrap();
}
