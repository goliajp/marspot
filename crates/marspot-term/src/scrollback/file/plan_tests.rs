//! The behaviour of segments, one test per state a process can leave
//! them in, and per thing they must not do.
//!
//! Segments here are small -- 4 KiB of records growing to 16 KiB, so a
//! full one holds 256 lines (its index is the limit) -- so a few hundred
//! lines go through every change there is.

use super::*;
use crate::grid::{Cell, PromptMark};
use crate::scrollback::testing::*;
use crate::segment::HEADER_BYTES;

pub(super) const COLS: usize = 8;
pub(super) const SMALL: Sizes = Sizes { first: 4096, full: 16384 };

/// Line `i`, saying so.
pub(super) fn row(i: u64) -> Vec<Cell> {
    format!("{i:08}").chars().map(|ch| Cell { ch, ..Default::default() }).collect()
}

pub(super) fn number(line: &[Cell]) -> u64 {
    line.iter().take(8).map(|c| c.ch).collect::<String>().parse().expect("a numbered line")
}

/// Segments of the kill test: large enough that one takes several of
/// the writer's buffers, so that a kill can leave one part written.  A
/// buffer holds at most one segment change, so segments smaller than a
/// buffer always end, on disk, where a buffer ended.
const KILLED: Sizes = Sizes { first: 64 << 10, full: 256 << 10 };

/// Line `i` of the flood the kill test runs: numbered, and of a length
/// that changes from line to line, so that what a buffer holds is not a
/// whole number of segments' worth of lines.
fn ragged(i: u64) -> Vec<Cell> {
    format!("{i:08}{}", "x".repeat((i % 50) as usize)).chars().map(|ch| Cell { ch, ..Default::default() }).collect()
}

pub(super) fn open(tmp: &TmpDir) -> FileScrollback {
    FileScrollback::open_sized(tmp.bin(), tmp.idx(), COLS, 4, SMALL).expect("open")
}

pub(super) fn push(sb: &mut FileScrollback, pushed: &mut u64) {
    sb.push_line(&row(*pushed), false);
    *pushed += 1;
}

/// The first logical line still held.
pub(super) fn held_from(sb: &FileScrollback) -> usize {
    sb.legacy.first().map(|l| l.first_line).or_else(|| sb.segs.first().map(|t| t.first_line)).unwrap_or(0) as usize
}

/// Every line held is the one after the line before it, the last is the
/// last of `pushed`, and every line before the first held reads as gone;
/// returns the first held line's number.
pub(super) fn whole(sb: &FileScrollback, pushed: u64) -> u64 {
    let lo = held_from(sb);
    let first = number(&sb.read_line(lo).expect("the oldest line held"));
    assert_eq!(first + (sb.len() - lo) as u64, pushed, "lines are missing at the end");
    for j in lo..sb.len() {
        let line = sb.read_line(j).unwrap_or_else(|| panic!("line {j} of {} reads back", sb.len()));
        assert_eq!(number(&line), first + (j - lo) as u64, "line {j}");
    }
    for j in 0..lo {
        assert!(sb.read_line(j).is_none(), "line {j} is past the budget and still reads");
    }
    first
}

pub(super) fn full_in_use(sb: &FileScrollback) -> usize {
    sb.segs.iter().filter(|t| t.seg.hdr.data == SMALL.full).count()
}

/// Push until two full segments are in use: the budget is reached.
pub(super) fn until_steady(sb: &mut FileScrollback, pushed: &mut u64) {
    while full_in_use(sb) < 2 {
        push(sb, pushed);
    }
}

/// Push until the next segment, asked for, has been made (or failed to
/// be).
pub(super) fn until_made(sb: &mut FileScrollback, pushed: &mut u64) {
    while sb.pending.is_none() {
        push(sb, pushed);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !sb.pending.as_ref().expect("asked for").is_made() {
        assert!(std::time::Instant::now() < deadline, "the next segment was never made");
        std::thread::yield_now();
    }
}

/// The directory's entries: name, inode, size.
pub(super) fn entries(dir: &std::path::Path) -> Vec<(String, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            // deleted since the directory was read
            let m = e.metadata().ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), m.ino(), m.len()))
        })
        .collect();
    v.sort();
    v
}

/// The directory's entries once what was handed over to be deleted is
/// gone: at most `want` of them, or whatever is left after a while.
pub(super) fn settled(dir: &std::path::Path, want: usize) -> Vec<(String, u64, u64)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let e = entries(dir);
        if e.len() <= want || std::time::Instant::now() > deadline {
            return e;
        }
        std::thread::yield_now();
    }
}

/// Hold the writer thread inside a write until the returned sender is
/// used or dropped.
pub(super) fn stall(sb: &mut FileScrollback) -> std::sync::mpsc::Sender<()> {
    let (release, stuck) = std::sync::mpsc::channel::<()>();
    sb.bin.borrow_mut().cut(move |_| {
        let _ = stuck.recv();
    });
    sb.hand_off();
    release
}

/// Case 1 of the plan: Records whose index entries were never written are not lines: the
/// lines are the entries, the next line goes after them, and every line
/// read back is a whole one.
#[test]
fn records_without_their_index_entries_are_not_lines() {
    use std::os::unix::fs::FileExt;
    let tmp = TmpDir::new("plan-1");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    for _ in 0..50 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    let hot = &sb.segs.last().unwrap().seg;
    assert_eq!(sb.segs.len(), 1, "the fixture stays in one segment");
    // the records of the last five are there, their entries are not
    hot.file.write_all_at(&[0u8; 40], hot.hdr.index_at() + 45 * 8).unwrap();
    drop(sb);
    let mut sb = open(&tmp);
    assert_eq!(sb.len(), 45, "lines are the entries, not the records");
    let mut pushed = 45;
    for _ in 0..10 {
        push(&mut sb, &mut pushed);
    }
    drop(sb);
    let sb = open(&tmp);
    assert_eq!(whole(&sb, 55), 0);
}

/// Case 2 of the plan: A segment used again: the entries its earlier use left past what
/// this use wrote are not lines.
#[test]
fn an_index_entry_of_an_earlier_use_is_not_a_line() {
    let tmp = TmpDir::new("plan-2");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_steady(&mut sb, &mut pushed);
    let reused = sb.segs.iter().find(|t| t.seg.hdr.data == SMALL.full).unwrap().seg.slot;
    while sb.segs.last().unwrap().seg.slot != reused {
        push(&mut sb, &mut pushed);
    }
    for _ in 0..9 {
        push(&mut sb, &mut pushed);
    }
    drop(sb);
    let sb = open(&tmp);
    assert_eq!(sb.segs.last().unwrap().seg.slot, reused);
    assert_eq!(sb.hot_lines, 10, "the line that changed segments and nine after it");
    whole(&sb, pushed);
}

/// Case 3 of the plan: A process killed in a segment change before the new header was
/// written leaves the history as it was before the change.
#[test]
fn a_segment_change_cut_before_its_header_keeps_the_old_order() {
    let tmp = TmpDir::new("plan-3");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_made(&mut sb, &mut pushed);
    while sb.hot_lines + 3 < sb.segs.last().unwrap().seg.hdr.index / 8 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    let (hot, before) = (sb.segs.last().unwrap().seg.slot, pushed);
    let release = stall(&mut sb);
    for _ in 0..6 {
        push(&mut sb, &mut pushed);
    }
    assert_eq!(sb.segs.len(), 2, "the fixture has to change segments");
    // killed: the writer never gets past the stall
    std::mem::forget(release);
    std::mem::forget(sb);
    let sb = open(&tmp);
    assert_eq!(sb.segs.len(), 1, "the segment changed to is not in use");
    assert_eq!(sb.segs[0].seg.slot, hot, "the hot segment is the one before the change");
    assert_eq!(whole(&sb, before), 0);
}

/// Case 4 of the plan: Killed after the new header and before a record: the segment
/// changed to is the hot one and empty, the one before is cold, and what
/// the segment held in its earlier use is gone.
#[test]
fn a_segment_change_cut_after_its_header_starts_an_empty_segment() {
    let tmp = TmpDir::new("plan-4");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_steady(&mut sb, &mut pushed);
    let reused = sb.segs[0].seg.slot;
    while sb.hot_lines + 1 < sb.segs.last().unwrap().seg.hdr.index / 8 {
        push(&mut sb, &mut pushed);
    }
    // the hot segment's last line, then the one that changes segments
    push(&mut sb, &mut pushed);
    sb.flush_for_handoff();
    let before = pushed;
    let (release, stalled) = std::sync::mpsc::channel();
    sb.hooks.stall_after_change = Some(stalled);
    push(&mut sb, &mut pushed);
    assert_eq!(sb.segs.last().unwrap().seg.slot, reused, "the fixture has to change segments");
    let serial = sb.epoch;
    sb.hand_off();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !crate::segment::list(&tmp.path, "scrollback", crate::segment::Kind::Scrollback, 20)
        .unwrap()
        .used
        .iter()
        .any(|s| s.slot == reused && s.hdr.serial == serial)
    {
        assert!(std::time::Instant::now() < deadline, "the new header was never written");
        std::thread::yield_now();
    }
    std::mem::forget(release);
    std::mem::forget(sb);
    let sb = open(&tmp);
    let table: Vec<_> = sb.segs.iter().map(|t| (t.seg.slot, t.seg.hdr.serial, t.lines)).collect();
    assert_eq!(sb.segs.last().unwrap().seg.slot, reused, "the segment changed to is the hot one: {table:?}");
    assert_eq!(sb.hot_lines, 0, "and holds no line yet: {table:?}, reused {reused}");
    assert_eq!(sb.segs.len(), 2);
    let first = whole(&sb, before);
    assert_eq!(first + sb.segs[0].lines, before, "what the reused segment held is gone");
}

/// Case 5 of the plan: Marks and cluster text go through the writer after their line and
/// come back with it, across segments and across a reopen; a line marked
/// twice reads as the second mark.
#[test]
fn a_note_follows_its_line_through_the_writer() {
    let tmp = TmpDir::new("plan-5");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    let marks = [(3u64, PromptMark::PromptStart), (70, PromptMark::InputStart), (71, PromptMark::OutputStart)];
    let clusters = |i: u64| vec![(1u16, format!("e\u{301}{i}")), (5u16, "\u{1F1EF}\u{1F1F5}".to_string())];
    while pushed < 200 {
        push(&mut sb, &mut pushed);
        let i = pushed - 1;
        if i == 3 {
            sb.mark_last_line(PromptMark::CommandEnd(Some(1)));
        }
        if let Some((_, m)) = marks.iter().find(|(at, _)| *at == i) {
            sb.mark_last_line(*m);
        }
        if i % 31 == 5 {
            sb.cluster_last_line(&clusters(i));
        }
    }
    assert!(sb.segs.len() >= 3, "the fixture spans segments");
    let check = |sb: &FileScrollback| {
        for i in 0..200u64 {
            let want = marks.iter().find(|(at, _)| *at == i).map_or(PromptMark::None, |(_, m)| *m);
            assert_eq!(sb.mark_at(i as usize), want, "the mark of line {i}");
            let mut got = Vec::new();
            let any = sb.clusters_at(i as usize, &mut got);
            if i % 31 == 5 {
                assert!(any && got == clusters(i), "the clusters of line {i}: {got:?}");
            } else {
                assert!(!any && got.is_empty(), "line {i} has no clusters");
            }
        }
        whole(sb, 200);
    };
    check(&sb);
    drop(sb);
    check(&open(&tmp));
}

/// Case 6 of the plan: A line whose note was still in a buffer when the process died
/// reads without it, and the lines after it keep their own.
#[test]
fn a_line_whose_note_was_cut_reads_without_it() {
    let tmp = TmpDir::new("plan-6");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    for _ in 0..10 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    sb.mark_last_line(PromptMark::PromptStart);
    // killed with the mark in the buffer
    std::mem::forget(sb);
    let mut sb = open(&tmp);
    push(&mut sb, &mut pushed);
    sb.mark_last_line(PromptMark::InputStart);
    push(&mut sb, &mut pushed);
    let check = |sb: &FileScrollback| {
        assert_eq!(sb.mark_at(9), PromptMark::None, "line 9's mark was never written");
        assert_eq!(sb.mark_at(10), PromptMark::InputStart);
        assert_eq!(sb.mark_at(11), PromptMark::None);
        whole(sb, 12);
    };
    check(&sb);
    drop(sb);
    check(&open(&tmp));
}

/// Case 7 of the plan: A process killed while making the next segment ready, after any of
/// the steps but the last, leaves a file the next open deletes; the
/// history is untouched.
#[test]
fn a_segment_half_prepared_is_removed_by_the_next_open() {
    for upto in 1..crate::segment::STEPS.len() {
        let tmp = TmpDir::new("plan-7");
        let mut sb = open(&tmp);
        sb.hooks.prep_upto = Some(upto);
        let mut pushed = 0;
        until_made(&mut sb, &mut pushed);
        let slot = sb.next_slot - 1;
        drop(sb);
        let half = crate::segment::path(&tmp.path, "scrollback", slot);
        assert!(half.exists(), "after {upto} steps: the fixture leaves the file");
        let sb = open(&tmp);
        assert!(!half.exists(), "after {upto} steps: the half-made segment was left");
        assert_eq!(whole(&sb, pushed), 0, "after {upto} steps");
    }
}

/// Case 8 of the plan: A segment made ready and never used is the next one used, by the
/// next process too: nothing new is made for that change.
#[test]
fn a_prepared_segment_never_used_is_kept_for_use() {
    let tmp = TmpDir::new("plan-8");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_made(&mut sb, &mut pushed);
    let slot = sb.next_slot - 1;
    drop(sb);
    let mut sb = open(&tmp);
    assert_eq!(sb.spares.iter().map(|s| s.slot).collect::<Vec<_>>(), [slot], "kept for use");
    while sb.segs.len() < 2 {
        push(&mut sb, &mut pushed);
    }
    assert_eq!(sb.segs[1].seg.slot, slot, "the change took it");
    assert_eq!(entries(&tmp.path).len(), 2, "and made nothing new");
    assert_eq!(whole(&sb, pushed), 0);
}

/// Case 9 of the plan: When the second full segment comes into use the smaller ones are
/// deleted; a process killed part way through that leaves the rest to
/// the next open; the lines inside the budget are all there.
#[test]
fn a_small_segment_past_the_budget_is_deleted() {
    let tmp = TmpDir::new("plan-9");
    let mut sb = open(&tmp);
    sb.hooks.evict_upto = Some(1);
    let mut pushed = 0;
    let small: Vec<_> = {
        until_steady(&mut sb, &mut pushed);
        (0..2).map(|slot| crate::segment::path(&tmp.path, "scrollback", slot)).collect()
    };
    assert!(!small[0].exists() && small[1].exists(), "the fixture dies after one deletion");
    whole(&sb, pushed);
    drop(sb);
    let mut sb = open(&tmp);
    assert!(!small[1].exists(), "the next open deleted the rest");
    whole(&sb, pushed);
    for _ in 0..600 {
        push(&mut sb, &mut pushed);
    }
    assert_eq!(settled(&tmp.path, 2).len(), 2, "two full segments and nothing else");
    whole(&sb, pushed);
}

/// Case 10 of the plan: At the budget the oldest full segment is used again in place:
/// the files, their inodes and sizes stay what they are; the lines it
/// held read as gone, and the lines inside the budget as they were.
#[test]
fn the_oldest_full_segment_is_overwritten_in_place() {
    let tmp = TmpDir::new("plan-10");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    until_steady(&mut sb, &mut pushed);
    let at_budget = settled(&tmp.path, 2);
    assert_eq!(at_budget.len(), 2);
    let first_before = whole(&sb, pushed);
    for _ in 0..1000 {
        push(&mut sb, &mut pushed);
    }
    sb.flush_for_handoff();
    assert_eq!(entries(&tmp.path), at_budget, "files were made, deleted or resized");
    let first_after = whole(&sb, pushed);
    assert!(first_after > first_before + 500, "lines past the budget were let go");
    assert!(pushed - first_after >= 256, "at least a full segment of lines is kept");
}

/// What `a_process_killed_mid_flood_loses_a_bounded_tail` runs in a
/// process of its own: numbered lines into the history named by the
/// environment, as fast as they go, until it is killed.
#[test]
#[ignore = "run by a_process_killed_mid_flood_loses_a_bounded_tail"]
fn pushes_lines_until_killed() {
    let Ok(dir) = std::env::var("MARSPOT_TEST_KILL_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    let mut sb = FileScrollback::open_sized(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 58, 4, KILLED)
        .expect("create");
    for i in 0u64..=u64::MAX {
        sb.push_line(&ragged(i), false);
        if i % 512 == 511 {
            // how far it has certainly got
            let _ = std::fs::write(dir.join("pushed"), (i + 1).to_string());
        }
    }
}

/// Case 15 of the plan: A process killed while it floods, with no flush and no
/// destructor, every few hundred lines changing segments: what it
/// leaves opens to lines in order up to the last, and what is missing
/// at the end is no more than the buffers that may be in flight.
#[test]
fn a_process_killed_mid_flood_loses_a_bounded_tail() {
    for kill_at in [20_000u64, 50_000] {
        let tmp = TmpDir::new("plan-15");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "scrollback::file::plan_tests::pushes_lines_until_killed", "--ignored", "--nocapture"])
            .env("MARSPOT_TEST_KILL_DIR", &tmp.path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("run the pusher");
        let pushed = || std::fs::read_to_string(tmp.path.join("pushed")).ok().and_then(|s| s.parse::<u64>().ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut certain = 0;
        while certain < kill_at {
            assert!(std::time::Instant::now() < deadline, "the pusher got to {certain} lines");
            certain = certain.max(pushed().unwrap_or(0));
            std::thread::yield_now();
        }
        child.kill().expect("kill the pusher");
        let _ = child.wait();
        certain = certain.max(pushed().unwrap_or(0));
        let sb = FileScrollback::open_sized(tmp.bin(), tmp.idx(), 58, 4, KILLED).expect("reopen");
        let first = whole(&sb, number(&sb.read_line(sb.len() - 1).unwrap()) + 1);
        let kept = first + (sb.len() - held_from(&sb)) as u64;
        // a full segment of these lines holds 192 to 256 of them
        assert!(kept - first >= 192, "killed at {kill_at}: less than a full segment is left");
        // the shortest record of these lines: header, one run of eight
        let record = (FILE_REC_HEADER_BYTES + 2 + 1 + 16 + COLS) as u64;
        let lost = certain.saturating_sub(kept);
        assert!(lost * record <= ((BIN_DEPTH + 1) * BIN_BUF_BYTES) as u64, "killed at {kill_at}: {lost} of {certain} lost");
    }
}

/// The record a test reads at `off` of segment `t`.
pub(super) fn first_record_len(t: &tiers::SegTier) -> u32 {
    u32::from_le_bytes(t.bytes(HEADER_BYTES, 4).unwrap().try_into().unwrap())
}
