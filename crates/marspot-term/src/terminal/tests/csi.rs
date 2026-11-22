//! Cursor movement, save and restore, scroll regions, line and character
//! edits, and the device reports.

use super::*;

#[test]
fn plain_ascii_writes_cells_and_advances_cursor() {
    let t = term_with(80, 24, b"hi");
    assert_eq!(t.grid().cell(0, 0).ch, 'h');
    assert_eq!(t.grid().cell(1, 0).ch, 'i');
    assert_eq!(t.grid().cursor(), (2, 0));
}

#[test]
fn esc_7_8_save_and_restore_cursor() {
    // DECSC (ESC 7) saves, DECRC (ESC 8) restores. The cursor at
    // save time gets stamped back even after intervening movement.
    let t = term_with(20, 5, b"\x1b[3;6H\x1b7\x1b[1;1H\x1b8");
    // CUP 3;6 = row 3 col 6 (1-indexed) → (5, 2) 0-indexed.
    assert_eq!(t.grid().cursor(), (5, 2));
}

#[test]
fn csi_s_u_save_and_restore_cursor() {
    // SCO variant — CSI s / CSI u — must behave identically to
    // ESC 7 / ESC 8 for the cursor.
    let t = term_with(20, 5, b"\x1b[2;3H\x1b[s\x1b[5;5H\x1b[u");
    // Should be back at (col=2, row=1).
    assert_eq!(t.grid().cursor(), (2, 1));
}

#[test]
fn restore_without_prior_save_is_a_noop() {
    // xterm semantics: CSI u / ESC 8 with no save = no-op, cursor
    // stays where it is. (Some terminals reset to home; xterm doesn't.)
    let t = term_with(20, 5, b"\x1b[2;3H\x1b[u\x1b8");
    assert_eq!(t.grid().cursor(), (2, 1));
}

#[test]
fn decstbm_sets_scroll_region_and_homes_cursor() {
    // CSI 3;7 r — region rows 3..=7 (1-indexed → 2..=6 0-indexed).
    // Cursor should move to (0, 0) per xterm.
    let t = term_with(20, 10, b"\x1b[5;5H\x1b[3;7r");
    assert_eq!(t.grid().cursor(), (0, 0));
    // Verify the region took effect via LF behaviour: cursor at
    // row 6 (0-indexed) is the scroll_bot — next LF scrolls
    // within the region, not the whole grid.
}

#[test]
fn lf_at_scroll_bot_scrolls_region_only() {
    // Set region 0..=2 (rows 1..=3 1-indexed). Fill rows 0, 1, 2,
    // 3 with distinctive markers; verify after LF at row 2 the
    // row 3 content is intact (didn't scroll with region).
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1b[1;3r"); // region [0..=2]
    t.feed(b"\x1b[1;1HAA\r\n"); // row 0 "AA"
    t.feed(b"BB\r\n"); // row 1 "BB"
    t.feed(b"CC\r\n"); // row 2 "CC", then LF triggers region scroll
    t.feed(b"DD"); // ???
    // After the LF after writing "CC", cursor was at row 2 (scroll_bot),
    // region scrolls up. Row 0 "AA" pushed off the region (NOT into
    // scrollback because region != full grid). Row 1 "BB" → row 0,
    // row 2 "CC" → row 1, row 2 blanked. Then "DD" written at row 2.
    assert_eq!(
        t.grid().cell(0, 0).ch,
        'B',
        "row 0 should be BB after scroll"
    );
    assert_eq!(
        t.grid().cell(0, 1).ch,
        'C',
        "row 1 should be CC after scroll"
    );
    assert_eq!(
        t.grid().cell(0, 2).ch,
        'D',
        "row 2 should be DD (new content)"
    );
    // Row 3 (outside region) should be unchanged from initial (blank).
    assert_eq!(
        t.grid().cell(0, 3).ch,
        ' ',
        "row 3 is outside region, unchanged"
    );
}

#[test]
fn il_inserts_blank_lines_at_cursor() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1b[1;1HAA\r\n"); // row 0
    t.feed(b"BB\r\n"); // row 1
    t.feed(b"CC\r\n"); // row 2
    t.feed(b"\x1b[2;1H"); // cursor to row 1
    t.feed(b"\x1b[L"); // IL: insert 1 line at row 1
    // Row 0 unchanged. Row 1 blank. Row 2 used to be BB → now is BB
    // (shifted down). Row 3 used to be CC → now CC.
    assert_eq!(t.grid().cell(0, 0).ch, 'A');
    assert_eq!(t.grid().cell(0, 1).ch, ' ', "inserted blank");
    assert_eq!(t.grid().cell(0, 2).ch, 'B');
    assert_eq!(t.grid().cell(0, 3).ch, 'C');
}

#[test]
fn dl_deletes_lines_at_cursor() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"AA\r\nBB\r\nCC\r\nDD\r\n");
    t.feed(b"\x1b[2;1H"); // cursor to row 1
    t.feed(b"\x1b[M"); // DL: delete 1 line at row 1
    assert_eq!(t.grid().cell(0, 0).ch, 'A');
    assert_eq!(t.grid().cell(0, 1).ch, 'C', "row 2 (CC) shifted up");
    assert_eq!(t.grid().cell(0, 2).ch, 'D', "row 3 (DD) shifted up");
    assert_eq!(t.grid().cell(0, 3).ch, ' ', "row 3 blanked");
}

#[test]
fn da_csi_c_queues_xterm_compatible_response() {
    // CSI c — Primary Device Attributes. App is asking "what
    // terminal class are you?". We respond with a VT220-compatible
    // attr string so apps that probe capabilities at startup get
    // an answer and don't fall back to degraded rendering.
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b[c");
    let resp = t.take_response();
    assert_eq!(resp, b"\x1b[?1;2c");
    // Second take should return empty (state was consumed).
    let resp2 = t.take_response();
    assert!(resp2.is_empty());
}

#[test]
fn el_when_cursor_on_wide_trail_clears_lead() {
    // 2026-06-15 "横线残留" repro.  Wide glyph at col 0-1 (中
    // as lead, '\0' trail — guaranteed wide via the EAW table,
    // independent of MARSPOT_AMBIGUOUS_WIDE).  CUP then
    // positions cursor onto col 1 (the trail slot) — happens
    // when an app's wcwidth disagrees with marspot's stored
    // width for that codepoint.  EL mode=0 then fills [1..10)
    // — without boundary repair the lead at col 0 stays intact,
    // rendering as a wide glyph spilling into the now-blank
    // trail.  Boundary repair pulls start back to col 0.
    let mut t = Terminal::new(10, 3);
    t.feed("中".as_bytes());
    t.feed(b"\x1b[1;2H");
    assert_eq!(t.grid().cursor(), (1, 0));
    t.feed(b"\x1b[K");
    assert_eq!(t.grid().cell(0, 0).ch, ' ', "wide-lead orphan not cleared");
    assert_eq!(t.grid().cell(1, 0).ch, ' ', "wide-trail not cleared");
}

#[test]
fn ed_partial_into_wide_lead_clears_trail() {
    // Mirror case for the right edge of fill_range.  Wide glyph
    // at col 4-5 (中 — unconditionally wide), cursor at col 6.
    // CUP to col 4 then EL mode=1 (erase from begin to cursor
    // inclusive).  Cursor cell is the wide LEAD at col 4.
    let mut t = Terminal::new(10, 3);
    t.feed(b"    ");
    t.feed("中".as_bytes());
    t.feed(b"\x1b[1;5H");
    t.feed(b"\x1b[1K");
    assert_eq!(
        t.grid().cell(4, 0).ch,
        ' ',
        "wide-lead under cursor not cleared"
    );
    assert_eq!(t.grid().cell(5, 0).ch, ' ', "wide-trail orphan not cleared");
}

/// DSR / CPR: `CSI 6 n` must answer with the cursor's 1-based
/// position, and `CSI 5 n` with a bare OK.  An app that asks and
/// is ignored waits forever — readline does this to redraw a
/// wrapped prompt — and a writer outside the terminal has no way
/// to tell "consumed my bytes" from "dropped them" without it.
#[test]
fn dsr_reports_cursor_position_and_status() {
    let mut t = Terminal::new(20, 5);
    t.feed(b"abc"); // cursor now col 3, row 0
    t.feed(b"\x1b[6n");
    assert_eq!(t.take_response(), b"\x1b[1;4R".to_vec());
    t.feed(b"\x1b[2;7H\x1b[6n"); // move, then ask again
    assert_eq!(t.take_response(), b"\x1b[2;7R".to_vec());
    t.feed(b"\x1b[5n");
    assert_eq!(t.take_response(), b"\x1b[0n".to_vec());
    // An unknown DSR parameter is silently ignored, not answered
    // with a malformed report.
    t.feed(b"\x1b[99n");
    assert!(t.take_response().is_empty());
}

#[test]
fn da1_burst_tracking_records_per_response() {
    // Five DA1 queries inside the parser's single feed call should
    // surface five responses in pending_response (concatenated)
    // and tick the burst window to five entries.  Exercises the
    // record_response bookkeeping without depending on log sink
    // state — the visible assert is just that the response bytes
    // were queued the expected number of times.
    let mut t = Terminal::new(20, 5);
    // Five consecutive DA1 queries.  Each is a complete escape so
    // the parser dispatches independently.
    t.feed(b"\x1b[c\x1b[c\x1b[c\x1b[c\x1b[c");
    let resp = t.take_response();
    assert_eq!(resp, b"\x1b[?1;2c".repeat(5), "expected 5 concatenated DA1 responses");
}

#[test]
fn deccm_cursor_key_app_mode_toggle() {
    // CSI ?1 h sets DECCKM (cursor key application mode); CSI ?1 l
    // clears it. The input layer reads this to decide arrow-key
    // encoding (ESC O X vs ESC [ X).
    let mut t = Terminal::new(20, 5);
    assert!(!t.cursor_key_application_mode());
    t.feed(b"\x1b[?1h");
    assert!(t.cursor_key_application_mode());
    t.feed(b"\x1b[?1l");
    assert!(!t.cursor_key_application_mode());
}

#[test]
fn ich_dch_ech_chars() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"ABCDEFG");
    // ICH 2 at col 2 — insert 2 blanks at col 2.
    t.feed(b"\x1b[1;3H"); // cursor (col=2, row=0)
    t.feed(b"\x1b[2@");
    // Was "ABCDEFG"; after ICH 2 at col 2: "AB  CDEF" (G falls off rhs)
    assert_eq!(t.grid().cell(0, 0).ch, 'A');
    assert_eq!(t.grid().cell(1, 0).ch, 'B');
    assert_eq!(t.grid().cell(2, 0).ch, ' ');
    assert_eq!(t.grid().cell(3, 0).ch, ' ');
    assert_eq!(t.grid().cell(4, 0).ch, 'C');

    // DCH 2 at col 2 — delete the 2 blanks; "ABCDEF" comes back.
    t.feed(b"\x1b[1;3H\x1b[2P");
    assert_eq!(t.grid().cell(0, 0).ch, 'A');
    assert_eq!(t.grid().cell(1, 0).ch, 'B');
    assert_eq!(t.grid().cell(2, 0).ch, 'C');
    assert_eq!(t.grid().cell(3, 0).ch, 'D');

    // ECH 2 at col 0 — erase chars in place, no shift.
    t.feed(b"\x1b[1;1H\x1b[2X");
    assert_eq!(t.grid().cell(0, 0).ch, ' ');
    assert_eq!(t.grid().cell(1, 0).ch, ' ');
    assert_eq!(t.grid().cell(2, 0).ch, 'C', "no shift");
}

#[test]
fn save_restore_round_trips_sgr_attrs() {
    // The save snapshot includes SGR — restoring brings back the
    // attrs at save time, even after later SGR changes. Verified
    // by colouring a glyph after restore and reading the cell back.
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b[31m"); // red foreground
    t.feed(b"\x1b[2;2H"); // cursor (1, 1)
    t.feed(b"\x1b7"); // save (cursor + attrs)
    t.feed(b"\x1b[34m"); // change to blue
    t.feed(b"\x1b[5;5HX"); // write X in blue at (4, 4)
    t.feed(b"\x1b8"); // restore (cursor → (1,1), attrs → red)
    t.feed(b"Y"); // write Y at (1, 1) in red
    let y = t.grid().cell(1, 1);
    let x = t.grid().cell(4, 4);
    assert_eq!(y.ch, 'Y');
    assert_eq!(x.ch, 'X');
    // The exact Color repr matters less than that they differ —
    // both should be palette colours, with Y in red (3, 31) and
    // X in blue (4, 34). The SGR restore should make y.attrs.fg !=
    // x.attrs.fg.
    assert_ne!(y.attrs.fg, x.attrs.fg);
}

#[test]
fn lf_only_advances_row_does_not_reset_column() {
    // xterm default (LNM unset): LF moves down only.  CR is needed to
    // return to column 0.  Many real shells emit CRLF together.
    let t = term_with(80, 24, b"abc\n");
    assert_eq!(t.grid().cursor(), (3, 1));
}

#[test]
fn crlf_returns_to_next_line_start() {
    let t = term_with(80, 24, b"abc\r\n");
    assert_eq!(t.grid().cursor(), (0, 1));
}

#[test]
fn csi_a_moves_cursor_up() {
    let t = term_with(80, 24, b"\n\n\n\x1B[2A");
    // After 3 LFs cursor is at row 3, col 0.  CUU 2 → row 1.
    assert_eq!(t.grid().cursor(), (0, 1));
}

#[test]
fn csi_b_moves_cursor_down() {
    let t = term_with(80, 24, b"\x1B[3B");
    assert_eq!(t.grid().cursor(), (0, 3));
}

#[test]
fn csi_c_moves_cursor_right() {
    let t = term_with(80, 24, b"\x1B[4C");
    assert_eq!(t.grid().cursor(), (4, 0));
}

#[test]
fn csi_d_moves_cursor_left() {
    let t = term_with(80, 24, b"abcdef\x1B[3D");
    assert_eq!(t.grid().cursor(), (3, 0));
}

#[test]
fn csi_no_param_moves_by_one() {
    let t = term_with(80, 24, b"\x1B[B");
    assert_eq!(t.grid().cursor(), (0, 1));
}

#[test]
fn csi_zero_param_treated_as_default() {
    // Param `0` and omitted are both "default" → 1 for cursor moves.
    let t = term_with(80, 24, b"\x1B[0B");
    assert_eq!(t.grid().cursor(), (0, 1));
}

#[test]
fn csi_h_no_params_homes_cursor() {
    let t = term_with(80, 24, b"abc\n\n\x1B[H");
    assert_eq!(t.grid().cursor(), (0, 0));
}

#[test]
fn csi_h_with_row_and_col_positions_cursor() {
    // CSI 3 ; 5 H → row 3, col 5 (1-indexed) → (col=4, row=2) 0-indexed.
    let t = term_with(80, 24, b"\x1B[3;5H");
    assert_eq!(t.grid().cursor(), (4, 2));
}

#[test]
fn csi_f_alias_for_cup() {
    let t = term_with(80, 24, b"\x1B[3;5f");
    assert_eq!(t.grid().cursor(), (4, 2));
}

#[test]
fn csi_g_sets_horizontal_position_absolute() {
    let t = term_with(80, 24, b"\n\x1B[10G");
    // VPA preserved at row 1; col = 10 (1-indexed) = 9 (0-indexed).
    assert_eq!(t.grid().cursor(), (9, 1));
}

#[test]
fn csi_d_lowercase_sets_vertical_position_absolute() {
    let t = term_with(80, 24, b"abc\x1B[5d");
    // HPA preserved at col 3; row = 5 (1-indexed) = 4 (0-indexed).
    assert_eq!(t.grid().cursor(), (3, 4));
}

#[test]
fn cursor_clamps_at_top_left() {
    let t = term_with(80, 24, b"\x1B[A\x1B[D");
    // CUU and CUB at origin must not underflow; cursor stays at (0,0).
    assert_eq!(t.grid().cursor(), (0, 0));
}

#[test]
fn cursor_clamps_at_bottom_right() {
    // CUP off-grid: 99;99 gets clamped to (rows-1, cols-1).  Then CUF /
    // CUD must not move further.
    let t = term_with(80, 24, b"\x1B[99;99H\x1B[10C\x1B[10B");
    assert_eq!(t.grid().cursor(), (79, 23));
}

#[test]
fn print_after_cursor_move_writes_at_new_position() {
    // Move cursor then print: cell at the moved-to position must hold
    // the printed glyph, not the original origin.
    let t = term_with(80, 24, b"\x1B[5;3HX");
    assert_eq!(t.grid().cell(2, 4).ch, 'X');
    assert_eq!(t.grid().cursor(), (3, 4));
}
