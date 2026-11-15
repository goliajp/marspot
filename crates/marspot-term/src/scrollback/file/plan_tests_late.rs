//! The behaviour of segments, continued: making the next one slower
//! than output or not at all, clearing, reflow, a search over a segment
//! used again, and the steady state.

use super::plan_tests::*;
use super::*;
use crate::grid::{Cell, PromptMark};
use crate::scrollback::testing::*;

/// Case 11 of the plan: Output faster than the next segment is made: the push that needs
/// it waits for it, and no line inside the budget is given up.
#[test]
fn output_faster_than_preparation_waits_and_loses_nothing() {
    let tmp = TmpDir::new("plan-11");
    let mut sb = open(&tmp);
    let (release, gate) = std::sync::mpsc::channel();
    sb.hooks.prep_gate = Some(gate);
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen = std::sync::Arc::clone(&progress);
    let pusher = std::thread::spawn(move || {
        let mut pushed = 0;
        while pushed < 300 {
            push(&mut sb, &mut pushed);
            seen.store(pushed, std::sync::atomic::Ordering::Relaxed);
        }
        sb
    });
    // the first segment holds 64 lines: the 65th needs the next one
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while progress.load(std::sync::atomic::Ordering::Relaxed) < 64 {
        assert!(std::time::Instant::now() < deadline, "the pusher did not get to the change");
        std::thread::yield_now();
    }
    release.send(()).unwrap();
    let sb = pusher.join().unwrap();
    assert_eq!(whole(&sb, 300), 0, "every line is there");
}

/// Case 12 of the plan: A disk that has no room for the next segment: the history goes
/// round in the segment it has, the pane goes on, and nothing of the
/// size asked for is left behind.
#[test]
fn a_full_disk_stops_growth_not_the_pane() {
    let tmp = TmpDir::new("plan-12");
    let mut sb = open(&tmp);
    sb.hooks.prep_bytes = Some(1 << 50);
    let mut pushed = 0;
    for _ in 0..300 {
        push(&mut sb, &mut pushed);
    }
    assert_eq!(sb.len(), 300);
    let first = whole(&sb, 300);
    assert!(first > 0, "the first segment went round instead of growing");
    assert!(entries(&tmp.path).iter().all(|(_, _, len)| *len < 1 << 30), "a huge file was left");
}

/// Case 13 of the plan: A clear cut short after it has put the older segments out of use
/// leaves only the hot segment's lines; a whole one leaves none.
#[test]
fn clear_cut_short_leaves_no_line_it_should_not() {
    let tmp = TmpDir::new("plan-13");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    while sb.segs.len() < 3 {
        push(&mut sb, &mut pushed);
    }
    for _ in 0..5 {
        push(&mut sb, &mut pushed);
    }
    let hot_lines = sb.hot_lines;
    sb.hooks.clear_upto = Some(1);
    sb.clear();
    drop(sb);
    let mut sb = open(&tmp);
    assert_eq!(sb.len() as u64, hot_lines, "only the hot segment's lines are left");
    whole(&sb, pushed);
    sb.clear();
    assert_eq!(sb.len(), 0);
    drop(sb);
    let sb = open(&tmp);
    assert_eq!(sb.len(), 0, "a whole clear leaves nothing");
}

/// Case 14 of the plan: A reflow starts a new use of the hot segment: lines of the old
/// width never come back, nor their notes on the new lines.
#[test]
fn reflow_starts_a_new_use_of_the_hot_segment() {
    let tmp = TmpDir::new("plan-14");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    while pushed < 100 {
        push(&mut sb, &mut pushed);
        sb.mark_last_line(PromptMark::PromptStart);
    }
    let Ok(mut sb) = sb.reopen_at_cols(16) else { panic!("reflowed") };
    let wide = |i: u64| -> Vec<Cell> { format!("{i:016}").chars().map(|ch| Cell { ch, ..Default::default() }).collect() };
    for i in 0..10 {
        sb.push_line(&wide(i), false);
    }
    let check = |sb: &FileScrollback| {
        assert_eq!(sb.len(), 10, "lines of the old width are gone");
        for i in 0..10 {
            assert_eq!(sb.read_line(i as usize).unwrap(), wide(i));
            assert_eq!(sb.mark_at(i as usize), PromptMark::None, "line {i} took an old line's mark");
        }
    };
    check(&sb);
    drop(sb);
    check(&FileScrollback::open_sized(tmp.bin(), tmp.idx(), 16, 4, SMALL).unwrap());
}

/// Case 18 of the plan: A search reading a segment the writer uses again finds its lines
/// gone, never the new lines in their places.
#[test]
fn a_search_over_a_segment_being_reused_reports_lines_gone() {
    let tmp = TmpDir::new("plan-18");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_steady(&mut sb, &mut pushed);
    let snap = sb.snapshot_for_search().unwrap();
    let (old, newer) = (&sb.segs[0], &sb.segs[1]);
    let (old_slot, old_first, newer_first) = (old.seg.slot, old.first_line, newer.first_line);
    assert_eq!(snap.search_line(old_first).map(|l| number(&l)), Some(old_first), "the view reads before");
    while sb.segs.last().unwrap().seg.slot != old_slot {
        push(&mut sb, &mut pushed);
    }
    for _ in 0..20 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    for line in old_first..old_first + 20 {
        assert_eq!(snap.search_line(line), None, "line {line} of a segment used again");
    }
    assert_eq!(snap.search_line(newer_first).map(|l| number(&l)), Some(newer_first), "the other segment still reads");
}

/// Case 22 of the plan: Once the budget is reached, writing creates, renames, deletes,
/// grows and truncates nothing: the directory's entries and its own
/// modification time stay as they are.
#[test]
fn steady_state_makes_no_name_changing_call() {
    let tmp = TmpDir::new("plan-22");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_steady(&mut sb, &mut pushed);
    for _ in 0..300 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    let before = (settled(&tmp.path, 2), std::fs::metadata(&tmp.path).unwrap().modified().unwrap());
    let changes = sb.next_serial;
    for _ in 0..2000 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    assert!(sb.next_serial >= changes + 5, "the fixture changes segments");
    let after = (entries(&tmp.path), std::fs::metadata(&tmp.path).unwrap().modified().unwrap());
    assert_eq!(after, before);
}
