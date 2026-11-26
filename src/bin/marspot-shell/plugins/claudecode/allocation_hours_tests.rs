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

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
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
    let rows = sum(&facts, H18, H18 + HOUR, H18 - HOUR);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.known), "{rows:?}");
    assert!(near(rows[0].fable, 0.1) && near(rows[0].other, 0.1), "{rows:?}");
    assert!(near(rows[0].busy_h, 0.25 + 2400.0 / 3600.0), "481 until it rests, 428 through its move: {rows:?}");
    assert_eq!(rows[0].panes, 2);
    assert!(near(rows[1].fable, 0.1) && near(rows[1].other, 0.0) && near(rows[1].busy_h, 0.5), "{rows:?}");
}

/// The shell went down at about 18:20 and came back at 21:30: 19:00 and
/// 20:00 have no fact and are unknown, not idle; 481, busy when it went
/// down, counts busy to the old shell's last fact and again from 21:30,
/// with no new busy fact (the new shell restores its state silently).
#[test]
fn allocation_hours_an_hour_the_shell_missed_is_unknown_and_a_busy_pane_counts_on_both_sides_of_it() {
    let opus = "model=claude-opus-5-5 in=0 cc5m=0 cc1h=0 cr=0 out=152500";
    let facts = [
        ev(H18 + 60, 481, "busy", "plain", ""),
        ev(H18 + 1200, 481, "used", "plain", opus),
        ev(H18 + 3 * HOUR + 1900, 481, "rest", "plain", ""),
        ev(H18 + 3 * HOUR + 2000, 430, "busy", "plain", ""),
    ];
    let up = H18 + 3 * HOUR + 1800;
    let rows = sum(&facts, H18, H18 + 3 * HOUR, up);
    assert_eq!(rows.iter().map(|r| r.known).collect::<Vec<_>>(), [true, false, false, true], "{rows:?}");
    assert!(near(rows[0].busy_h, 1140.0 / 3600.0), "481 to 18:20, not through the gap: {rows:?}");
    assert!(near(rows[1].busy_h, 0.0) && near(rows[2].busy_h, 0.0), "{rows:?}");
    assert!(near(rows[3].busy_h, (100.0 + 1600.0) / 3600.0), "481 from the start to its rest, 430 to the hour's end: {rows:?}");
    assert_eq!(rows[1].line(), format!("{}\thour\tfable=- other=- busy_h=- panes=-\n", H18 + HOUR));
    // an idle hour the shell started in is known
    let rows = sum(&facts[..3], H18, H18 + 3 * HOUR, H18 + 2 * HOUR + 600);
    assert!(rows[2].known && !rows[1].known, "{rows:?}");
    // the same facts with the shell up all along: 481 busy through the gap
    let rows = sum(&facts, H18, H18 + 3 * HOUR, H18);
    assert!(rows.iter().all(|r| r.known) && near(rows[1].busy_h, 1.0), "{rows:?}");
}

#[test]
fn allocation_hours_append_each_hour_once_catch_up_after_a_gap_and_keep_sixty_days() {
    let root = dir("tick");
    let events = EventLog::new(root.join("events"));
    let hours_dir = root.join("hours");
    let mut log = EventLog::new(root.join("events"));
    log.append(&ev(H18 + 60, 1, "busy", "plain", "")).unwrap();
    // the first shell to write the files sums back two days from the facts
    let mut h = Hours::new(hours_dir.clone());
    assert_eq!(h.tick(&events, H18 + HOUR + 5).unwrap(), CATCH_UP as usize, "back to 11-24T19:00");
    assert_eq!(h.tick(&events, H18 + HOUR + 900).unwrap(), 0, "once");
    // the shell was down for three hours; a new one finds where the file ends
    let mut h = Hours::new(hours_dir.clone());
    assert_eq!(h.tick(&events, H18 + 4 * HOUR + 1).unwrap(), 3);
    let text = fs::read_to_string(hours_dir.join("202611.tsv")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<i64> = lines.iter().map(|l| l.split('\t').next().unwrap().parse().unwrap()).collect();
    let want: Vec<i64> = (0..CATCH_UP + 3).map(|k| H18 - (CATCH_UP - 1) * HOUR + k * HOUR).collect();
    assert_eq!(starts, want);
    assert!(lines[0].ends_with("\thour\tfable=- other=- busy_h=- panes=-"), "before any fact: {text}");
    // its busy began before this shell and nothing of it was seen after
    assert!(lines[CATCH_UP as usize - 1].ends_with("\thour\tfable=0.0000 other=0.0000 busy_h=0.000 panes=1"), "{text}");
    assert!(lines[CATCH_UP as usize..].iter().all(|l| l.ends_with("busy_h=- panes=-")), "the three hours down: {text}");
    fs::write(hours_dir.join("202608.tsv"), "").unwrap();
    fs::write(hours_dir.join("202609.tsv"), "").unwrap();
    let mut h = Hours::new(hours_dir.clone());
    // 2026-11-30: sixty days back is 2026-10-01, so September goes
    let late = H18 + 4 * DAY;
    h.tick(&events, late).unwrap();
    assert!(!hours_dir.join("202608.tsv").exists() && !hours_dir.join("202609.tsv").exists());
    let text = fs::read_to_string(hours_dir.join("202611.tsv")).unwrap();
    assert_eq!(text.lines().count(), 2 * CATCH_UP as usize + 3, "a long gap is summed back two days at most");
    fs::remove_dir_all(&root).unwrap();
}
