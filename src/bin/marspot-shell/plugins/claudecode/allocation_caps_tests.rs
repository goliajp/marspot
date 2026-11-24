//! Which moves count against the limits, in a dry run and live, and
//! across a change of mode and a restart.

use std::fs;

use super::super::super::allocation_events::{Event, EventLog};
use super::*;

const NOW: i64 = 1_795_530_644;

fn ev(at: i64, kind: &str, detail: &str) -> Event {
    Event { at, sid: 1, kind: kind.into(), profile: Some(2), class: "plain".into(), detail: detail.into() }
}

/// (hourly band, the pane's week)
fn counts(a: &Allocation) -> (usize, usize) {
    (a.band_moves.len(), a.last.get(&1).map_or(0, |l| l.rest_moves.len()))
}

#[test]
fn allocation_caps_count_would_moves_in_a_dry_run_and_moves_made_live() {
    let dir = std::env::temp_dir().join(format!("marspot-alloc-caps-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut main = EventLog::new(dir.clone());
    main.append(&ev(NOW - 3000, "bound", "")).unwrap();
    // two would-moves in the dry run, and one move of the pane's made
    main.append(&ev(NOW - 2000, "would_move", "to=P1 why=plan acc=")).unwrap();
    main.append(&ev(NOW - 1000, "would_move", "to=P3 why=rescue acc=")).unwrap();
    main.append(&ev(NOW - 500, "moved", "from=P2 why=leave_early among=P1")).unwrap();
    // a move nothing chose by plan counts against neither
    main.append(&ev(NOW - 400, "moved", "from=P2 why=out")).unwrap();

    let mut a = Allocation::new(dir.clone());
    a.rebuild(NOW);
    assert_eq!(counts(&a), (2, 2), "dry run: what live would have done");

    a.set_live(true, NOW);
    assert_eq!(counts(&a), (0, 1), "live: only the move made; leaving early is no band move");
    main.append(&ev(NOW + 5, "moved", "from=P1 why=plan acc=")).unwrap();
    main.append(&ev(NOW + 6, "would_move", "to=P3 why=plan acc=")).unwrap();
    a.count_made(NOW + 10);
    assert_eq!(counts(&a), (1, 2), "a move the main thread made since; a would-move is not one");
    a.count_made(NOW + 12);
    assert_eq!(counts(&a), (1, 2), "each counted once");

    a.set_live(false, NOW + 20);
    assert_eq!(counts(&a), (3, 3), "back to the dry run: every would-move of the last hour and week");

    // a restart while live counts the moves made, not the would-moves
    let mut again = Allocation::new(dir.clone());
    again.set_live(true, NOW + 30);
    again.rebuild(NOW + 30);
    assert_eq!(counts(&again), (1, 2));
    fs::remove_dir_all(&dir).unwrap();
}
