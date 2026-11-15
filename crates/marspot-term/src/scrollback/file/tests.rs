//! Reading and writing lines, and the files of the format before
//! segments.

use super::plan_tests::{COLS, SMALL, number, open, push, row, whole};
use super::*;
use crate::grid::{Cell, PromptMark};
use crate::scrollback::Scrollback;
use crate::scrollback::testing::*;
use crate::segment::HEADER_BYTES;

/// Case 16 of the plan: The pairs of the format before segments -- a v2 cold pair and a
/// v3 or v4 hot one, with marks -- are the oldest history: their lines
/// first, then the new ones, every one as written, marks too.  They go
/// when the budget passes them, and not before.
#[test]
fn old_hot_and_cold_files_read_as_the_oldest_history() {
    for hot_version in [3, 4] {
        let tmp = TmpDir::new("plan-16");
        let cold: Vec<(Vec<Cell>, bool)> = (0..20).map(|i| (rich_row(i, 12), i % 3 == 0)).collect();
        let hot: Vec<(Vec<Cell>, bool)> = (20..50).map(|i| (rich_row(i, 12), i % 4 == 0)).collect();
        let (cold_bin, cold_idx) = (tmp.bin().with_extension("cold.bin"), tmp.idx().with_extension("cold.idx"));
        write_v2_pair(&cold_bin, &cold_idx, &cold);
        write_legacy_pair(&tmp.bin(), &tmp.idx(), hot_version, &hot, 77);
        mark_legacy(&tmp.bin(), 77, 2, PromptMark::PromptStart);
        let old_bytes = std::fs::read(tmp.bin()).unwrap();
        let mut sb = FileScrollback::open_sized(tmp.bin(), tmp.idx(), 12, 4, SMALL).expect("open");
        assert_eq!(sb.len(), 50, "v{hot_version}: the old lines are there");
        let new: Vec<(Vec<Cell>, bool)> = (100..140).map(|i| (rich_row(i, 12), i % 5 == 0)).collect();
        for (r, w) in &new {
            sb.push_line(r, *w);
        }
        let check = |sb: &FileScrollback| {
            for (i, (r, w)) in cold.iter().chain(&hot).chain(&new).enumerate() {
                assert_eq!(&sb.read_line(i).unwrap()[..], &r[..], "v{hot_version}: line {i}");
                assert_eq!(sb.wrapped_at(i), *w, "v{hot_version}: wrapped {i}");
            }
            assert_eq!(sb.mark_at(22), PromptMark::PromptStart, "v{hot_version}: the old pair's mark");
            assert_eq!(sb.mark_at(23), PromptMark::None);
        };
        check(&sb);
        drop(sb);
        assert_eq!(std::fs::read(tmp.bin()).unwrap(), old_bytes, "v{hot_version}: the old files are not written");
        let mut sb = FileScrollback::open_sized(tmp.bin(), tmp.idx(), 12, 4, SMALL).expect("reopen");
        check(&sb);
        // inside the budget they stay; past it they go
        assert!(sb.segs.len() < 3, "v{hot_version}: the fixture is inside the budget");
        assert!(tmp.bin().exists() && cold_bin.exists(), "v{hot_version}: deleted inside the budget");
        for i in 0..600 {
            sb.push_line(&rich_row(2000 + i, 12), false);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while tmp.bin().exists() || cold_bin.exists() || tmp.bin().with_extension("bin.marks").exists() {
            assert!(std::time::Instant::now() < deadline, "v{hot_version}: the old files were never deleted");
            std::thread::yield_now();
        }
        assert!(sb.read_line(0).is_none(), "v{hot_version}: their lines are gone with them");
    }
}

/// Case 17 of the plan: A build from before segments, opened on a directory that has
/// them, makes its own files: none of its names is a segment's, and the
/// segments are left as they were for when the newer build is back.
#[test]
fn an_older_binary_does_not_destroy_segments() {
    let tmp = TmpDir::new("plan-17");
    let mut sb = open(&tmp);
    let mut pushed = 0;
    for _ in 0..150 {
        push(&mut sb, &mut pushed);
    }
    drop(sb);
    let segs: Vec<_> = std::fs::read_dir(&tmp.path).unwrap().map(|e| e.unwrap().path()).collect();
    let before: Vec<Vec<u8>> = segs.iter().map(|p| std::fs::read(p).unwrap()).collect();
    // every name the older format writes, rotation and quarantine too
    let mut theirs = legacy::all_paths(&tmp.bin(), &tmp.idx());
    for ext in ["next.bin", "next.idx", "new.bin", "bin.old", "cold.bin.old"] {
        theirs.push(tmp.bin().with_extension(ext));
    }
    for p in &segs {
        assert!(!theirs.contains(p), "{} is a name the older format uses", p.display());
        assert!(!p.file_name().unwrap().to_string_lossy().contains(".corrupt-"));
    }
    // the older build writes its pair
    let old: Vec<(Vec<Cell>, bool)> = (0..5).map(|i| (row(9000 + i), false)).collect();
    write_legacy_pair(&tmp.bin(), &tmp.idx(), 4, &old, 5);
    for (p, b) in segs.iter().zip(&before) {
        assert_eq!(&std::fs::read(p).unwrap(), b, "{} was changed", p.display());
    }
    // and the newer one, back, reads both
    let sb = open(&tmp);
    assert_eq!(sb.len(), 155);
    for i in 0..5 {
        assert_eq!(number(&sb.read_line(i).unwrap()), 9000 + i as u64);
    }
    for i in 5..155 {
        assert_eq!(number(&sb.read_line(i).unwrap()), i as u64 - 5);
    }
}

/// An image that replaces itself runs no destructors: after the flush
/// every line is in the files, the index as well as the records,
/// however little of a buffer they filled.
#[test]
fn every_line_is_indexed_on_disk_before_a_handoff() {
    let tmp = TmpDir::new("handoff-idx");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 4).expect("create");
    // more than the ring holds, less than either buffer
    for i in 0..1000usize {
        sb.push_line(&fill(b'a' + (i % 26) as u8, COLS), false);
    }
    sb.flush_for_handoff();
    std::mem::forget(sb);
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 4).expect("reopen");
    assert_eq!(sb.len(), 1000);
    assert_eq!(sb.cell_at(999, 0).map(|c| c.ch), Some((b'a' + (999 % 26) as u8) as char));
    assert_eq!(sb.cell_at(0, 0).map(|c| c.ch), Some('a'));
}

/// A few lines and then silence: they reach the files without a flush
/// and without the buffers filling.
#[test]
fn a_short_burst_reaches_the_files_once_handed_off() {
    let tmp = TmpDir::new("burst-end");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 4).expect("create");
    for _ in 0..3 {
        sb.push_line(&fill(b'x', COLS), false);
    }
    sb.hand_off();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while FileScrollback::on_disk(&tmp.bin(), &tmp.idx()).unwrap().0 < 3 {
        assert!(std::time::Instant::now() < deadline, "the burst never reached the files");
        std::thread::yield_now();
    }
    assert_eq!(FileScrollback::on_disk(&tmp.bin(), &tmp.idx()).unwrap().0, 3);
}

/// CSI 3 J reaches the files: a reopen sees nothing from before the
/// clear, and lines pushed after it persist.
#[test]
fn clear_persists() {
    let tmp = TmpDir::new("clear");
    let mut sb = Scrollback::file(tmp.bin(), tmp.idx(), 4, 8).expect("open");
    for i in 0..20u32 {
        sb.push_line_with_wrapped(&fill(b'a' + (i % 26) as u8, 4), false);
    }
    sb.clear();
    assert_eq!(sb.len(), 0, "cleared in-process");
    sb.push_line_with_wrapped(&fill(b'z', 4), false);
    sb.flush_for_handoff();
    drop(sb);
    let sb = Scrollback::file(tmp.bin(), tmp.idx(), 4, 8).expect("reopen");
    assert_eq!(sb.len(), 1, "reopen must see only post-clear content");
    assert_eq!(sb.cell_at(0, 0).map(|c| c.ch), Some('z'));
}

/// Trailing default cells are not stored: the record holds the prefix,
/// which reads back at its columns, and past it a read is `None`, which
/// the renderer shows as a blank.
#[test]
fn push_line_trims_default_tail() {
    let tmp = TmpDir::new("trim-tail");
    let (cols, prefix_len) = (200usize, 30usize);
    let mut line: Vec<Cell> = (0..prefix_len).map(|i| Cell { ch: (b'a' + (i % 26) as u8) as char, ..Default::default() }).collect();
    line.resize(cols, Cell::default());
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("create");
    sb.push_line(&line, false);
    for _ in 0..6 {
        sb.push_line(&fill(b'.', cols), false);
    }
    sb.flush_for_handoff();
    // one run: count, kind, attributes, then a byte a cell
    assert_eq!(super::plan_tests::first_record_len(&sb.segs[0]) as usize, 1 + 2 + (2 + 1 + 16) + prefix_len);
    drop(sb);
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("reopen");
    for c in 0..prefix_len {
        assert_eq!(sb.cell_at(0, c).unwrap().ch, (b'a' + (c % 26) as u8) as char, "prefix col {c}");
    }
    for c in prefix_len..cols {
        assert!(sb.cell_at(0, c).is_none(), "trimmed tail col {c}");
    }
}

/// The trim compares whole cells: a blank that differs from the default
/// in any one byte -- a background colour, a single flag, the cluster
/// bit -- is content and stays.
#[test]
fn push_line_keeps_a_tail_cell_that_differs_in_any_byte() {
    use crate::grid::{CellAttrs, Color, FLAG_CLUSTER};
    let blank = |attrs: CellAttrs| Cell { ch: ' ', attrs };
    let tails = [
        blank(CellAttrs { bg: Color::indexed(4), ..Default::default() }),
        blank(CellAttrs { fg: Color::rgb(1, 2, 3), ..Default::default() }),
        blank(CellAttrs { dim: true, ..Default::default() }),
        blank(CellAttrs { _pad: [FLAG_CLUSTER, 0, 0], ..Default::default() }),
        Cell { ch: '\0', ..Default::default() },
    ];
    let tmp = TmpDir::new("trim-any-byte");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 64).expect("create");
    for tail in tails {
        let mut r = vec![Cell::default(); COLS];
        r[0] = Cell { ch: 'a', ..Default::default() };
        r[5] = tail;
        sb.push_line(&r, false);
    }
    drop(sb);
    // a ring of one: every row but the last is read from its record
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 1).expect("reopen");
    for i in 0..tails.len() - 1 {
        assert!(sb.cell_at(i, 5).is_some(), "row {i}: the tail cell was trimmed");
        assert_eq!(sb.cell_at(i, 6), None, "row {i}: the default cells after it were not");
    }
}

/// A segment change hands the writer thread its new file: it does not
/// end that thread and start another.
#[test]
fn a_segment_change_keeps_its_writer_thread() {
    let tmp = TmpDir::new("change-thread");
    let mut sb = open(&tmp);
    let writer = sb.bin.borrow().thread_id();
    let mut pushed = 0;
    for _ in 0..2000 {
        push(&mut sb, &mut pushed);
    }
    assert!(sb.next_serial > 6, "the fixture changes segments");
    assert_eq!(sb.bin.borrow().thread_id(), writer, "a change started another writer thread");
    whole(&sb, pushed);
}

/// Both read paths -- `cell_at` and `read_line` -- decode a history
/// whose old lines are v2 records and new ones are runs.
#[test]
fn both_read_paths_decode_a_history_of_mixed_widths() {
    let tmp = TmpDir::new("mixed-widths");
    let cols = 10;
    let old: Vec<(Vec<Cell>, bool)> = (0..8).map(|i| (rich_row(i, cols), false)).collect();
    write_v2_pair(&tmp.bin(), &tmp.idx(), &old);
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 2).expect("open");
    let new: Vec<(Vec<Cell>, bool)> = (50..58).map(|i| (rich_row(i, cols), false)).collect();
    for (r, w) in &new {
        sb.push_line(r, *w);
    }
    let all: Vec<_> = old.iter().chain(new.iter()).collect();
    for (i, (r, _)) in all.iter().enumerate().take(all.len() - 2) {
        for (c, want) in r.iter().enumerate() {
            assert_eq!(sb.cell_at(i, c).as_ref(), Some(want), "cell_at line {i} col {c}");
        }
        assert_eq!(&sb.read_line(i).unwrap()[..], &r[..], "read_line {i}");
    }
}

#[test]
fn a_line_round_trips() {
    let tmp = TmpDir::new("one-line");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 16).expect("create");
    sb.push_line(&fill(b'a', COLS), false);
    assert_eq!(sb.len(), 1);
    let got = sb.read_line(0).expect("read");
    assert_eq!(got.len(), COLS);
    assert_eq!(got[0].ch, 'a');
    assert_eq!(sb.cell_at(0, 3).unwrap().ch, 'a');
    assert!(!sb.wrapped_at(0));
}

/// `cell_at(line, col)` returns the cell at `col` of a line read from
/// its record, not the row's first cell.
#[test]
fn cell_at_off_ring_returns_correct_column() {
    let tmp = TmpDir::new("off-ring-col");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 4).expect("create");
    let first: Vec<Cell> = (0..COLS).map(|c| Cell { ch: (b'A' + c as u8) as char, ..Default::default() }).collect();
    sb.push_line(&first, false);
    for _ in 0..8 {
        sb.push_line(&fill(b'.', COLS), false);
    }
    for c in 0..COLS {
        assert_eq!(sb.cell_at(0, c).map(|x| x.ch), Some((b'A' + c as u8) as char), "col {c}");
    }
}

#[test]
fn the_wrapped_flag_survives_a_reopen() {
    let tmp = TmpDir::new("wrapped");
    {
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8).expect("create");
        for (b, w) in [(b'p', false), (b'q', true), (b'r', false), (b's', true)] {
            sb.push_line(&fill(b, 4), w);
        }
    }
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8).expect("reopen");
    assert_eq!(sb.len(), 4);
    assert_eq!((0..4).map(|i| sb.wrapped_at(i)).collect::<Vec<_>>(), [false, true, false, true]);
}

/// A line that has left the RAM ring but is still in the writer's
/// buffer is read after a flush, and only one flush is paid for a run
/// of such reads.
#[test]
fn a_read_past_the_ring_flushes_what_is_buffered() {
    let tmp = TmpDir::new("cold-flush");
    let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 16).expect("create");
    for i in 0..200usize {
        sb.push_line(&fill(b'a' + (i % 26) as u8, 4), false);
    }
    assert!(sb.has_unflushed.get());
    assert_eq!(sb.cell_at(5, 0).map(|c| c.ch), Some((b'a' + 5) as char));
    assert!(!sb.has_unflushed.get(), "the read flushed");
}

#[test]
fn many_lines_round_trip_through_a_reopen() {
    let tmp = TmpDir::new("many-reopen");
    {
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 16).expect("create");
        for i in 0..5000 {
            sb.push_line(&fill(b'a' + (i % 26) as u8, COLS), false);
        }
    }
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), COLS, 16).expect("reopen");
    assert_eq!(sb.len(), 5000);
    for i in [4999usize, 2500, 0] {
        assert_eq!(sb.cell_at(i, 0).unwrap().ch, (b'a' + (i % 26) as u8) as char, "line {i}");
    }
}

/// An old pair this build cannot read is an error for the caller to set
/// aside, not something to read past.
#[test]
fn an_old_pair_with_a_bad_header_is_an_error() {
    let tmp = TmpDir::new("corrupt-err");
    let mut bad = vec![0xffu8; 4];
    bad.resize(40, 0);
    std::fs::write(tmp.bin(), &bad).unwrap();
    std::fs::write(tmp.idx(), b"junk").unwrap();
    let err = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8).err().expect("open must fail on bad magic");
    assert!(format!("{err}").contains("magic"), "{err}");
}

#[test]
fn an_old_pair_with_a_ragged_index_is_an_error() {
    let tmp = TmpDir::new("idx-misaligned");
    let rows: Vec<(Vec<Cell>, bool)> = (0..5).map(|_| (fill(b'a', 4), false)).collect();
    write_legacy_pair(&tmp.bin(), &tmp.idx(), 4, &rows, 1);
    let mut idx = std::fs::read(tmp.idx()).unwrap();
    idx.push(0x55);
    std::fs::write(tmp.idx(), &idx).unwrap();
    let err = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8).err().expect("open must fail");
    assert!(format!("{err}").contains("multiple of 8"));
}

/// An old pair whose index runs past its records -- a kill with the
/// index ahead -- is read up to the last whole record, and the files
/// are not changed for it.
#[test]
fn an_old_pair_is_read_up_to_its_last_whole_record() {
    use std::io::Write;
    let tmp = TmpDir::new("past-eof-tail");
    let rows: Vec<(Vec<Cell>, bool)> = (0..10).map(|_| (fill(b'x', 4), false)).collect();
    write_legacy_pair(&tmp.bin(), &tmp.idx(), 4, &rows, 1);
    let bin_len = std::fs::metadata(tmp.bin()).unwrap().len();
    // a record whose length runs past the end, and entries past it
    std::fs::OpenOptions::new().append(true).open(tmp.bin()).unwrap().write_all(&[100, 0, 0, 0, 1, 4, 0]).unwrap();
    let mut idx = std::fs::OpenOptions::new().append(true).open(tmp.idx()).unwrap();
    idx.write_all(&bin_len.to_le_bytes()).unwrap();
    for i in 0..5u64 {
        idx.write_all(&(bin_len + 100 + i * 7).to_le_bytes()).unwrap();
    }
    drop(idx);
    let idx_before = std::fs::read(tmp.idx()).unwrap();
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8).expect("reopen");
    assert_eq!(sb.len(), 10);
    assert_eq!(sb.cell_at(9, 0).unwrap().ch, 'x');
    drop(sb);
    assert_eq!(std::fs::read(tmp.idx()).unwrap(), idx_before, "read-only");
}

/// A reflow through the enum the grid holds: the history starts over at
/// the new width, rather than the re-pushed lines landing on top of the
/// old ones.
#[test]
fn a_restart_starts_the_history_over_for_reflow() {
    let tmp = TmpDir::new("resize-no-dup");
    {
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 16).expect("create");
        for i in 0..50u32 {
            sb.push_line(&fill(b'a' + (i % 26) as u8, 4), false);
        }
    }
    let mut sb = Scrollback::file(tmp.bin(), tmp.idx(), 4, 16).expect("reopen as enum");
    assert_eq!(sb.len(), 50);
    sb.restart(8);
    assert_eq!(sb.len(), 0, "restart must clear the history");
    for i in 0..50u32 {
        sb.push_line_with_wrapped(&fill(b'a' + (i % 26) as u8, 8), false);
    }
    assert_eq!(sb.len(), 50, "50, not 100");
    assert_eq!(sb.line_to_vec(0).expect("row 0 decodes").len(), 8, "at the new width");
}

/// A fresh history's lines belong to a run of their own, and reopening
/// is not a new run: the lines are the same lines.
#[test]
fn reopening_keeps_the_run() {
    let tmp = TmpDir::new("epoch-reopen");
    let first = {
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
        assert_ne!(sb.epoch, 0);
        push_n(&mut sb, 3, 8, 'a');
        sb.flush_for_handoff();
        sb.epoch
    };
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("reopen");
    assert_eq!(sb.epoch, first);
    assert_eq!(sb.len(), 3);
}

/// What a fresh pane costs on disk before it has printed anything: one
/// segment of the first size, made at once.
#[test]
fn a_fresh_history_is_one_small_segment() {
    let tmp = TmpDir::new("fresh");
    let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
    let names: Vec<_> = std::fs::read_dir(&tmp.path).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["scrollback.seg.0"]);
    let len = std::fs::metadata(tmp.path.join("scrollback.seg.0")).unwrap().len();
    assert_eq!(len, HEADER_BYTES + (1 << 20) + (1 << 17));
    drop(sb);
}
