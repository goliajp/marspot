//! Wrapping and scrolling at the bottom row, the alt screen, and local-echo
//! prediction.

use super::*;

// ----- print path: wrap and BS -----

#[test]
fn print_at_eol_wraps_to_next_row() {
    let mut t = Terminal::new(5, 3);
    t.feed(b"abcdef"); // 5 chars fill row 0; 'f' wraps to row 1 col 0
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    assert_eq!(t.grid().cell(4, 0).ch, 'e');
    assert_eq!(t.grid().cell(0, 1).ch, 'f');
    assert_eq!(t.grid().cursor(), (1, 1));
}

#[test]
fn bs_decrements_column_and_does_not_erase() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"ab\x08"); // print ab, then BS
    assert_eq!(t.grid().cursor(), (1, 0));
    // BS is non-destructive — 'b' must remain.
    assert_eq!(t.grid().cell(1, 0).ch, 'b');
}

#[test]
fn bs_at_column_zero_is_clamped() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"\x08");
    assert_eq!(t.grid().cursor(), (0, 0));
    t.feed(b"\n\x08"); // LF then BS — col stays 0, row stays 1
    assert_eq!(t.grid().cursor(), (0, 1));
}

// ----- scrolling -----

#[test]
fn lf_at_bottom_row_scrolls_up_and_pushes_to_scrollback() {
    let mut t = Terminal::new(3, 2);
    t.feed(b"abc"); // row 0 full; DECAWM defers the wrap → cursor parks at last col.
    assert_eq!(t.grid().cursor(), (2, 0));
    // Manually park cursor at last row, last col, then LF.
    t.feed(b"\x1B[2;3H"); // CUP row 2 col 3 (1-indexed) → (col 2, row 1)
    t.feed(b"\n");
    // Row 0 ("abc") goes to scrollback, row 1 becomes blank.
    assert_eq!(t.grid().scrollback_len(), 1);
    let sb = t.grid().scrollback_line(0).unwrap();
    assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
    // Cursor stays on the (now blank) last row.
    assert_eq!(t.grid().cursor(), (2, 1));
    for c in 0..3 {
        assert_eq!(t.grid().cell(c, 1), Cell::default());
    }
}

#[test]
fn print_overflow_at_bottom_row_scrolls() {
    // Print enough to fill the entire 2-row grid; the next print must
    // trigger a scroll, not clamp.
    let mut t = Terminal::new(3, 2);
    t.feed(b"abcdefg"); // 6 chars fill the grid; 'g' triggers scroll
    // After scroll: scrollback contains "abc", visible row 0 = "def",
    // 'g' lands at (0, 1).
    assert_eq!(t.grid().scrollback_len(), 1);
    let sb = t.grid().scrollback_line(0).unwrap();
    assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
    assert_eq!(t.grid().cell(0, 0).ch, 'd');
    assert_eq!(t.grid().cell(2, 0).ch, 'f');
    assert_eq!(t.grid().cell(0, 1).ch, 'g');
    assert_eq!(t.grid().cursor(), (1, 1));
}

#[test]
fn scroll_inherits_current_bg_for_blank_row() {
    // BCE for scroll-fill: when SGR has bg=red and we scroll, the new
    // blank row at the bottom must carry that bg.
    let mut t = Terminal::new(3, 2);
    t.feed(b"\x1B[41m"); // bg red
    t.feed(b"abc\x1B[2;3H\n"); // park cursor at last row, LF → scroll
    for c in 0..3 {
        let cell = t.grid().cell(c, 1);
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.attrs.bg, Color::indexed(1));
    }
}

#[test]
fn csi_3_j_clears_scrollback_only() {
    // RFC-004 amendment (2026-07-17): CSI 3 J now clears the
    // PERSISTENT tier too (`clear` + app restart must stay
    // cleared) — File and Memory agree on len()==0 after 3J.
    // This test uses the Memory variant; the File-side
    // persistence contract is pinned in
    // scrollback::tests::clear_truncates_persistent_file.
    // Force Memory by clearing `MARSPOT_SESSION_ID`.
    // The var IS set when the suite runs inside a marspot
    // terminal (L3 exports it to the shell); without this
    // removal the test flips to File semantics and fails.
    let mut env = crate::test_env::EnvGuard::new();
    env.remove("MARSPOT_SESSION_ID");
    let mut t = Terminal::new(3, 2);
    // Build some scrollback by feeding many lines.
    for _ in 0..5 {
        t.feed(b"xxx\x1B[2;3H\n");
    }
    assert!(t.grid().scrollback_len() > 0);
    // ESC[3J clears scrollback; visible region untouched.
    t.feed(b"\x1B[3J");
    assert_eq!(t.grid().scrollback_len(), 0);
    // Visible row 0 should still hold what was there.
    assert_eq!(t.grid().cell(0, 0).ch, 'x');
}

// ----- soak: long-running scroll must not grow memory -----

use crate::platform::testing::current_rss_bytes;

#[test]
#[ignore = "soak; run via bin/soak.sh"]
fn soak_scrollback_bounded_under_ten_million_lines() {
    // Force the Memory variant — see csi_3_J test for why the
    // var can be present (suite run inside a marspot terminal).
    let mut env = crate::test_env::EnvGuard::new();
    env.remove("MARSPOT_SESSION_ID");
    let mut t = Terminal::new(80, 24);
    // Warm up enough to fully wrap the ring once so all backing
    // pages have been faulted in before we baseline.  The Memory
    // variant has DEFAULT_SCROLLBACK_LINES = 10 000 slots;
    // 40 000 warmup lines covers that with ~4x margin.
    // Without this the test's "baseline" lands mid-fill and
    // the next burst's lazy faults look like a leak.
    let warmup = b"\x1B[24;80H\n".repeat(40_000);
    t.feed(&warmup);

    let baseline = current_rss_bytes();

    // 10 M lines = ~7.8x the previous 1 M target.  At default
    // scrollback capacity (10 K lines) we wrap the ring ~1000
    // times — far past the point any per-write leak would
    // accumulate measurably.
    let line = b"this is a fairly typical 50-character log line!\n";
    let park = b"\x1B[24;80H";
    for _ in 0..10_000_000 {
        t.feed(line);
        // After each line, park cursor at last row so the next \n scrolls.
        t.feed(park);
    }

    let after = current_rss_bytes();
    let growth = after.saturating_sub(baseline);

    // Tolerance: 5 MB.  The ring is pre-allocated to capacity at
    // construction; once warmed up, additional lines reuse slots.
    const TOLERANCE: u64 = 5 * 1024 * 1024;
    assert!(
        growth < TOLERANCE,
        "scrollback ring grew {} bytes ({}→{}); expected bounded",
        growth,
        baseline,
        after
    );

    // Sanity: scrollback is exactly capped at the configured capacity.
    assert_eq!(
        t.grid().scrollback_len(),
        t.grid().scrollback_capacity(),
        "scrollback should be at capacity after the soak"
    );
}

#[test]
fn dec_1049_h_enters_alt_blank_grid() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"hello\r\n");
    assert!(first_row_text(&t).starts_with("hello"));

    t.feed(b"\x1b[?1049h"); // enter alt
    // Alt grid is fresh: row 0 should be all spaces.
    assert_eq!(first_row_text(&t).trim_end(), "");
}

#[test]
fn dec_1049_l_restores_main_contents_and_cursor() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"main_a\r\nmain_b");
    let cursor_before = t.grid().cursor();

    // Enter alt, write something only in alt.  The switch does not
    // move the cursor -- that is why `?1049l` has to restore it --
    // so the program's first line lands where it was standing, and
    // one that wants the top asks for it.
    t.feed(b"\x1b[?1049h");
    t.feed(b"\x1b[H");
    t.feed(b"alt_only");
    assert_eq!(first_row_text(&t).trim_end(), "alt_only");

    // Exit — main grid + cursor restored.
    t.feed(b"\x1b[?1049l");
    assert!(first_row_text(&t).starts_with("main_a"));
    assert_eq!(t.grid().cursor(), cursor_before);
}

#[test]
fn dec_25_l_hides_cursor_h_shows() {
    let mut t = Terminal::new(10, 5);
    assert!(t.cursor_visible());
    t.feed(b"\x1b[?25l");
    assert!(!t.cursor_visible());
    t.feed(b"\x1b[?25h");
    assert!(t.cursor_visible());
}

#[test]
fn alt_screen_ignored_if_already_in_alt() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"main\r\n");
    t.feed(b"\x1b[?1049h");
    t.feed(b"first_alt");
    // Re-entering must NOT clobber the saved main grid.
    t.feed(b"\x1b[?1049h");
    t.feed(b"\x1b[?1049l");
    // Main is still there.
    assert!(first_row_text(&t).starts_with("main"));
}

#[test]
fn alt_grid_resize_tracks_main() {
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b[?1049h"); // alt mode
    t.resize(30, 8);
    // Cursor must be in-bounds in the active (alt) grid.
    let (c, r) = t.grid().cursor();
    assert!(c < 30 && r < 8);
    // Saved main grid resized too — exit and verify it's the new size.
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.grid().cols(), 30);
    assert_eq!(t.grid().rows(), 8);
}

// -------- local-echo prediction tests --------------------------------

#[test]
fn predict_paints_cell_and_advances_cursor() {
    let mut t = Terminal::new(20, 3);
    assert!(t.predict_byte(b'a'));
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    assert_eq!(t.grid().cursor(), (1, 0));
    // Hit/miss counters: prediction is queued, neither yet.
    assert_eq!(t.predictions_hit, 0);
    assert_eq!(t.predictions_miss, 0);
}

#[test]
fn predict_then_matching_echo_is_silently_consumed() {
    let mut t = Terminal::new(20, 3);
    t.predict_byte(b'a');
    t.predict_byte(b'b');
    // PTY echoes both verbatim — grid stays the same, predictions
    // confirmed.
    t.feed(b"ab");
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    assert_eq!(t.grid().cell(1, 0).ch, 'b');
    assert_eq!(t.grid().cursor(), (2, 0));
    assert_eq!(t.predictions_hit, 2);
    assert_eq!(t.predictions_miss, 0);
}

#[test]
fn mismatch_rolls_back_all_predictions_then_feeds_byte() {
    let mut t = Terminal::new(20, 3);
    // Pre-state: cell(0,0) blank, cursor at (0,0).
    t.predict_byte(b'a');
    t.predict_byte(b'b');
    // PTY sends 'X' instead of expected 'a' — rollback both
    // predictions, then write 'X' at the original cursor.
    t.feed(b"X");
    assert_eq!(t.grid().cell(0, 0).ch, 'X');
    // The 'b' prediction's cell (col 1) is back to blank.
    assert_eq!(t.grid().cell(1, 0).ch, ' ');
    assert_eq!(t.grid().cursor(), (1, 0));
    assert_eq!(t.predictions_hit, 0);
    assert_eq!(t.predictions_miss, 2);
}

#[test]
fn silent_program_gives_the_screen_back_on_its_own() {
    // A `sudo` password prompt: echo off, and not one byte comes
    // back until Enter.  Without a deadline the guesses stay
    // painted — the password, in clear, on screen.
    let mut t = Terminal::new(20, 5);
    for b in b"hunter22" {
        assert!(t.predict_byte(*b));
    }
    assert_eq!(&first_row_text(&t)[..8], "hunter22");
    assert!(t.predictions_pending());

    let deadline = t.predict_deadline();
    assert!(!t.expire_predictions_at(Instant::now()), "not yet");
    assert!(t.expire_predictions_at(Instant::now() + deadline));

    assert_eq!(first_row_text(&t).trim_end(), "");
    assert_eq!(t.grid.cursor(), (0, 0));
    assert_eq!(t.predictions_expired, 8);
    // Nothing arrived, so nothing is decided yet: expiry alone is
    // not evidence the program refuses to echo.
    assert_eq!(t.predict_misses, 0);
}

#[test]
fn late_echo_widens_the_window_instead_of_counting_against_it() {
    // A real link: the echo is slower than the deadline but it
    // does come.  Losing local echo on exactly the connection it
    // was built for would be the wrong lesson to draw.
    let mut t = Terminal::new(20, 5);
    t.predict_byte(b'a');
    let deadline = t.predict_deadline();
    assert!(t.expire_predictions_at(Instant::now() + deadline));
    assert_eq!(first_row_text(&t).trim_end(), "");

    // The echo lands after the guess was taken back.
    t.feed(b"a");
    assert_eq!(&first_row_text(&t)[..1], "a");
    assert_eq!(t.predict_misses, 0, "a late echo is not a miss");
    assert!(t.can_predict(), "the link stays worth predicting on");
}

#[test]
fn the_window_is_learned_from_the_link_not_assumed() {
    // Whatever the round trip turns out to be, the deadline
    // follows it — the floor protects a local pty, the ceiling
    // keeps an unechoed keystroke from lingering.
    let mut t = Terminal::new(20, 5);
    assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MIN);
    for _ in 0..40 {
        t.note_echo_rtt(Duration::from_millis(300));
    }
    assert!(t.predict_deadline() > Duration::from_millis(900));
    for _ in 0..200 {
        t.note_echo_rtt(Duration::from_millis(5000));
    }
    assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MAX);
    for _ in 0..400 {
        t.note_echo_rtt(Duration::from_micros(160));
    }
    assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MIN);
}

#[test]
fn expired_then_something_else_is_a_real_miss() {
    // The password prompt again, now past Enter: what finally
    // arrives is sudo's own output, not the typed bytes.  Three
    // of those and the pane stops guessing.
    let mut t = Terminal::new(20, 5);
    for _ in 0..3 {
        assert!(t.predict_byte(b'x'));
        let deadline = t.predict_deadline();
        assert!(t.expire_predictions_at(Instant::now() + deadline));
        t.feed(b"\r\n");
    }
    assert_eq!(t.predict_misses, 3);
    assert!(!t.can_predict());
    assert!(!t.predict_byte(b'x'));
    assert_eq!(first_row_text(&t).trim_end(), "");
}

#[test]
fn predict_refused_in_alt_screen() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b[?1049h"); // enter alt screen
    assert!(!t.can_predict());
    assert!(!t.predict_byte(b'a'));
    // No grid mutation, no queue growth.
    assert_eq!(t.grid().cell(0, 0).ch, ' ');
    assert!(t.predictions.is_empty());
}

#[test]
fn predict_refused_for_control_chars() {
    let mut t = Terminal::new(20, 3);
    // Tab, CR, LF, BS, ESC all unsafe — shell-side meaning varies.
    for b in [b'\t', b'\r', b'\n', 0x08u8, 0x1Bu8, 0x03u8] {
        assert!(!t.predict_byte(b), "byte {b:#04x} should not predict");
    }
    assert!(t.predictions.is_empty());
}

#[test]
fn alt_screen_mid_feed_drops_pending_predictions() {
    let mut t = Terminal::new(20, 3);
    t.predict_byte(b'a');
    // PTY response opens alt screen — predictions are anchored to
    // the now-discarded main grid, so they get tossed.
    t.feed(b"\x1b[?1049h");
    assert!(t.predictions.is_empty());
    assert_eq!(t.predictions_miss, 1);
}
