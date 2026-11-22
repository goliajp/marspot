//! Cursor-line moves, the deferred wrap, index and reverse index, and reflow
//! on resize.

use super::*;

// ── Resize reflow gate ─────────────────────────────────────────
//
// Regression wall for the "resize 截断" class of bug: shrinking
// the window used to truncate every row at the narrow width and
// wipe scrollback, so a shrink → grow round trip (window drag,
// shell self-update reopening at the default rect, attach-then-
// resize bootstrap) permanently mangled content.  These tests pin
// the reflow contract: ANY resize sequence is lossless for
// logical content.

/// One visible row as a trimmed string ('\0' wide-trail cells
/// skipped, same as the clipboard serialiser).
/// 2026-07-28 field report — Homebrew's concurrent-download UI
/// scrolled forever instead of redrawing in place.  brew moves the
/// cursor with `\033[0G` + `\033[{n}F` (CPL); marspot's CSI
/// dispatch had no `F` arm, the sequence was silently dropped, and
/// every refresh APPENDED its lines.  This test replays brew's
/// exact redraw shape (Tty.move_cursor_beginning +
/// move_cursor_up_beginning from download_queue.rb).
#[test]
fn brew_style_cpl_redraw_updates_in_place() {
    let mut t = Terminal::new(40, 6);
    t.feed(b"openjdk 10%\r\nqemu 5%");
    // brew's refresh: CR to column 0, up (lines-1) to the first
    // status row, then rewrite both lines with EL after each.
    for pct in [20u32, 30, 40] {
        t.feed(b"\x1b[0G\x1b[1F");
        t.feed(format!("openjdk {pct}%\x1b[K\r\nqemu {pct}%\x1b[K").as_bytes());
    }
    assert_eq!(row_text(&t, 0), "openjdk 40%");
    assert_eq!(row_text(&t, 1), "qemu 40%");
    assert_eq!(row_text(&t, 2), "", "no appended garbage rows");
    assert_eq!(
        t.grid().scrollback_len(),
        0,
        "in-place redraw must not push anything into scrollback"
    );
}

/// CNL is CPL's mirror; pin both while we are here.
#[test]
fn cnl_moves_down_to_column_zero() {
    let mut t = Terminal::new(20, 5);
    t.feed(b"abc\x1b[2Exyz");
    assert_eq!(row_text(&t, 0), "abc");
    assert_eq!(row_text(&t, 2), "xyz", "down 2 rows, column 0");
    // Clamped at the bottom edge, never scrolls.
    t.feed(b"\x1b[99Eq");
    assert_eq!(row_text(&t, 4), "q");
    assert_eq!(t.grid().scrollback_len(), 0);
}

/// A coloured prompt wider than the pane used to lose its tail.
///
/// Verbatim from session 387's bytelog: oh-my-zsh in a 73-column
/// pane, with a branch name long enough that `) ✗ ` falls past the
/// right edge.  The SGR between the glyph that filled the last
/// column and the next glyph cleared the deferred wrap, so `✗` and
/// the space after it overwrote column 72 and vanished, leaving the
/// cursor stuck at the edge with nowhere to type.
#[test]
fn an_sgr_does_not_drop_the_deferred_wrap() {
    let t = term_with(
        73,
        6,
        b"\x1b[01;31m\xe2\x9e\x9c  \x1b[36minsight\x1b[00m \x1b[01;34mgit:(\
\x1b[31mfeature/QIP-90-asking-for-the-wall-clears-the-step-past\x1b[34m) \
\x1b[33m\xe2\x9c\x97\x1b[00m ",
    );
    assert_eq!(
        row_text(&t, 0),
        "\u{279c}  insight git:(feature/QIP-90-asking-for-the-wall-clears-the-step-past)"
    );
    assert_eq!(row_text(&t, 1), "\u{2717}", "the tail wrapped instead of being lost");
    assert_eq!(t.grid().cursor(), (2, 1));
}

/// The deferred wrap is cancelled per OPERATION, and the list is
/// ghostty's (`references/ghostty`, `src/terminal/Terminal.zig`):
/// cursor movement, the erases and the line/blank inserts clear it;
/// SGR, mode set/reset, device queries, OSC and save/restore keep
/// it.  Blanket-clearing on every CSI is what lost the prompt tail
/// above; blanket-KEEPING would wrap a glyph that a `CUP` had just
/// moved away from the edge.
#[test]
fn the_deferred_wrap_is_cancelled_per_operation() {
    // Fills the row exactly, arming the wrap; then the sequence
    // under test; then one more glyph.
    let probe = |seq: &[u8]| -> bool {
        let mut input = vec![b'x'; 20];
        input.extend_from_slice(seq);
        input.push(b'Z');
        let t = term_with(20, 4, &input);
        row_text(&t, 1).starts_with('Z')
    };
    for (name, seq) in [
        ("SGR", &b"\x1b[31m"[..]),
        ("SGR reset", b"\x1b[0m"),
        ("DECSET 2004", b"\x1b[?2004h"),
        ("DECRST 2004", b"\x1b[?2004l"),
        ("DA", b"\x1b[c"),
        ("OSC 133", b"\x1b]133;B\x07"),
        ("DECSC + DECRC", b"\x1b7\x1b8"),
        ("SU", b"\x1b[S"),
        ("BEL", b"\x07"),
    ] {
        assert!(probe(seq), "{name} must not cancel the deferred wrap");
    }
    for (name, seq) in [
        ("CR", &b"\r"[..]),
        ("LF", b"\n"),
        ("BS", b"\x08"),
        ("CUF", b"\x1b[C"),
        ("CUP", b"\x1b[1;1H"),
        ("EL", b"\x1b[K"),
        ("ED", b"\x1b[J"),
        ("IL", b"\x1b[L"),
        ("ICH", b"\x1b[@"),
    ] {
        assert!(!probe(seq), "{name} must cancel the deferred wrap");
    }
}

/// `ESC D` / `ESC E` / `ESC M` were falling through to the
/// catch-all, which is invisible until a program uses one to move
/// its own screen.  codex emits five `ESC M` to open room at the
/// top of its scroll region and then paints everything below at
/// absolute rows; with the five discarded its layout sat five rows
/// lower here than in its own model, and the next partial repaint
/// wrote the status line across a row that still held the old text
/// (`• Workingqu21e message`, 2026-09-08).
#[test]
fn reverse_index_scrolls_the_region_down_at_its_top() {
    // Region is the whole screen; cursor at the top row.
    let mut t = term_with(10, 4, b"a\r\nb\r\nc\r\nd\x1b[1;1H");
    assert_eq!(row_text(&t, 0), "a");
    t.feed(b"\x1bM");
    assert_eq!(row_text(&t, 0), "", "a blank row is opened at the top");
    assert_eq!(row_text(&t, 1), "a");
    assert_eq!(row_text(&t, 2), "b");
    assert_eq!(t.grid().cursor(), (0, 0), "the cursor stays at the top");
}

#[test]
fn reverse_index_below_the_top_just_moves_the_cursor() {
    let mut t = term_with(10, 4, b"a\r\nb\r\nc\x1b[3;2H");
    assert_eq!(t.grid().cursor(), (1, 2));
    t.feed(b"\x1bM");
    assert_eq!(t.grid().cursor(), (1, 1), "up one row, no scroll");
    assert_eq!(row_text(&t, 0), "a", "nothing moved");
}

#[test]
fn index_matches_a_line_feed_and_next_line_adds_the_return() {
    // IND at the bottom scrolls, exactly as LF does.
    let mut a = term_with(10, 3, b"1\r\n2\r\n3");
    let mut b = term_with(10, 3, b"1\r\n2\r\n3");
    a.feed(b"\n");
    b.feed(b"\x1bD");
    assert_eq!(row_text(&a, 0), row_text(&b, 0));
    assert_eq!(row_text(&a, 1), row_text(&b, 1));
    assert_eq!(a.grid().cursor(), b.grid().cursor(), "IND is LF without the CR");

    // NEL is IND plus the carriage return.
    let mut c = term_with(10, 4, b"1\r\n2\x1b[2;5H");
    assert_eq!(c.grid().cursor(), (4, 1));
    c.feed(b"\x1bE");
    assert_eq!(c.grid().cursor(), (0, 2), "column 0 of the next row");
}

/// The shape codex actually sends: a run of RIs against a region
/// that does not start at the screen top.
#[test]
fn reverse_index_respects_the_scroll_region() {
    // Region rows 2..4 (1-based), cursor parked at its top.
    let mut t = term_with(10, 5, b"a\r\nb\r\nc\r\nd\r\ne\x1b[2;4r\x1b[2;1H");
    t.feed(b"\x1bM");
    assert_eq!(row_text(&t, 0), "a", "the row above the region is untouched");
    assert_eq!(row_text(&t, 1), "", "a blank opens at the region's top");
    assert_eq!(row_text(&t, 2), "b");
    assert_eq!(row_text(&t, 3), "c");
    assert_eq!(row_text(&t, 4), "e", "the row below the region is untouched");
}

#[test]
fn autowrap_sets_continuation_flag() {
    let t = term_with(10, 5, b"0123456789ABCDE");
    assert!(!t.grid().row_wrapped(0));
    assert!(t.grid().row_wrapped(1), "autowrapped row must be flagged");
    assert_eq!(row_text(&t, 0), "0123456789");
    assert_eq!(row_text(&t, 1), "ABCDE");
    // Explicit newline does NOT flag.
    let t2 = term_with(10, 5, b"abc\r\ndef");
    assert!(!t2.grid().row_wrapped(1));
}

#[test]
fn reflow_shrink_rewraps_instead_of_truncating() {
    let mut t = term_with(10, 5, b"0123456789ABCDE");
    t.resize(5, 5);
    assert_eq!(row_text(&t, 0), "01234");
    assert_eq!(row_text(&t, 1), "56789");
    assert_eq!(row_text(&t, 2), "ABCDE");
    assert!(t.grid().row_wrapped(1) && t.grid().row_wrapped(2));
    assert_eq!(logical_text(&t), "0123456789ABCDE");
}

#[test]
fn reflow_grow_unwraps_back_to_one_row() {
    let mut t = term_with(10, 5, b"0123456789ABCDE");
    t.resize(5, 5);
    t.resize(20, 5);
    assert_eq!(row_text(&t, 0), "0123456789ABCDE");
    assert_eq!(row_text(&t, 1), "");
    assert!(!t.grid().row_wrapped(1));
    assert_eq!(logical_text(&t), "0123456789ABCDE");
}

#[test]
fn reflow_roundtrip_with_scrollback_is_lossless() {
    // 40 numbered lines through a 24-row grid → 16+ lines live in
    // scrollback.  Mix of short lines and >80-char lines so both
    // the wrap and no-wrap paths are exercised.
    let mut t = Terminal::new(80, 24);
    for i in 0..40 {
        let line = if i % 7 == 0 {
            format!("line{:02}-{}\r\n", i, "x".repeat(100))
        } else {
            format!("line{:02}\r\n", i)
        };
        t.feed(line.as_bytes());
    }
    let before = logical_text(&t);
    assert!(t.grid().scrollback_len() > 0, "test needs scrollback");
    t.resize(37, 24);
    assert_eq!(logical_text(&t), before, "shrink must be lossless");
    t.resize(80, 24);
    assert_eq!(logical_text(&t), before, "grow back must be lossless");
    t.resize(13, 24);
    t.resize(200, 50);
    t.resize(80, 24);
    assert_eq!(
        logical_text(&t),
        before,
        "wild resize chain must be lossless"
    );
}

#[test]
fn rows_only_resize_preserves_scrollback_and_content() {
    let mut t = Terminal::new(20, 5);
    for i in 0..12 {
        t.feed(format!("l{:02}\r\n", i).as_bytes());
    }
    let before = logical_text(&t);
    t.resize(20, 3);
    assert_eq!(logical_text(&t), before, "rows shrink must be lossless");
    t.resize(20, 10);
    assert_eq!(logical_text(&t), before, "rows grow must be lossless");
}

#[test]
fn reflow_never_splits_wide_pairs() {
    // 2 ASCII + 5 CJK (10 cols) + 2 ASCII = 14 columns at width 20.
    let mut t = term_with(20, 4, "AA你好世界啊BB".as_bytes());
    for w in [5u16, 7, 4, 20] {
        t.resize(w, 4);
        let g = t.grid();
        for r in 0..g.rows() {
            assert_ne!(
                g.cell(0, r).ch,
                '\0',
                "row {r} at width {w} starts with a wide-trail cell"
            );
        }
        for i in 0..g.scrollback_len() {
            assert_ne!(g.scrollback_line(i).unwrap()[0].ch, '\0');
        }
        assert_eq!(logical_text(&t), "AA你好世界啊BB");
    }
}

#[test]
fn reflow_keeps_cursor_on_its_logical_position() {
    // Prompt-like: short line, cursor right after it.
    let mut t = term_with(40, 10, b"$ echo hello");
    assert_eq!(t.grid().cursor(), (12, 0));
    t.resize(8, 10);
    // "$ echo hello" (12 chars) at width 8 → "$ echo h" / "ello";
    // cursor lands at the end of the continuation row.
    let (col, row) = t.grid().cursor();
    assert_eq!(row, 1);
    assert_eq!(col, 4);
    t.resize(40, 10);
    assert_eq!(t.grid().cursor(), (12, 0));
}

#[test]
fn ed2_clear_screen_resets_continuation_flags() {
    let mut t = term_with(10, 5, b"0123456789ABCDE");
    assert!(t.grid().row_wrapped(1));
    t.feed(b"\x1b[2J");
    for r in 0..5 {
        assert!(!t.grid().row_wrapped(r), "row {r} flag survived ED2");
    }
}

#[test]
fn reflow_preserves_blank_lines_between_content() {
    let mut t = term_with(20, 8, b"top\r\n\r\n\r\nbottom");
    let before = logical_text(&t);
    assert_eq!(before, "top\n\n\nbottom");
    t.resize(9, 8);
    t.resize(20, 8);
    assert_eq!(logical_text(&t), before);
}
