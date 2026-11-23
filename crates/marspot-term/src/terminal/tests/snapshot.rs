//! Snapshot round trips across every wire version, and the rejects.

use super::*;

#[test]
fn snapshot_roundtrip_empty_terminal() {
    // Fresh terminal: no feed, generation 0, all defaults.
    // Round-tripping a default state must produce a default state.
    let src = Terminal::new(20, 5);
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(1, 1); // intentionally different dims
    dst.apply_snapshot(&bytes).unwrap();
    assert_terms_equivalent(&src, &dst);
}

#[test]
fn snapshot_roundtrip_with_text_and_cursor() {
    // "Hello\nWorld" + a couple SGR runs: covers ascii cells,
    // cursor advancement, and a newline that scroll-region-aware.
    let src = term_with(20, 5, b"\x1b[31mHello\x1b[0m\r\nWorld");
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(20, 5);
    dst.apply_snapshot(&bytes).unwrap();
    assert_terms_equivalent(&src, &dst);
    // And the wire form is non-trivial — at least the header +
    // 5 rows × 20 cols × 13 = 1300 cells + ~50 bytes header.
    assert!(
        bytes.len() > 1000,
        "wire form suspiciously small: {}",
        bytes.len()
    );
}

#[test]
fn snapshot_roundtrip_with_cjk_wide_cells() {
    // 中文字符占 2 cells — wide pairing is a known fragile area.
    // After roundtrip, the lead cell must still hold the CJK
    // char + the trail must still hold the wide-sentinel.
    let src = term_with(10, 3, "中文测试".as_bytes());
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(10, 3);
    dst.apply_snapshot(&bytes).unwrap();
    assert_terms_equivalent(&src, &dst);
}

#[test]
fn snapshot_roundtrip_preserves_rgb_and_indexed_colors() {
    // SGR 38;2;r;g;b (RGB) + SGR 38;5;n (indexed) + default —
    // exercises all three Color variants in the per-cell attrs.
    let src = term_with(30, 3, b"\x1b[38;2;200;100;50mRGB\x1b[38;5;82mIDX\x1b[0mDEF");
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(30, 3);
    dst.apply_snapshot(&bytes).unwrap();
    assert_terms_equivalent(&src, &dst);
}

#[test]
fn snapshot_roundtrip_with_saved_cursor_and_modes() {
    // DECSC (ESC 7) saves cursor + attrs; DECSET ?1, ?2004, ?25
    // exercise the mode bitset path.  After roundtrip the
    // SavedCursor option + modes must survive.
    //
    // The DECSC here used to be written `ESC SP 7`, with a stray
    // space, and still saved a cursor because the dispatcher
    // ignored intermediates.  Once `ESC # 8` had to mean DECALN
    // rather than DECRC, intermediates started counting and this
    // fixture stopped saving anything -- which is the test having
    // been right about the roundtrip and wrong about the input.
    let src = term_with(20, 5, b"\x1b[31mAB\x1b7\x1b[?1h\x1b[?2004h\x1b[?25l");
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(20, 5);
    dst.apply_snapshot(&bytes).unwrap();
    assert_terms_equivalent(&src, &dst);
    assert!(dst.cursor_key_application_mode);
    assert!(dst.bracketed_paste_mode.on);
    assert!(!dst.cursor_visible);
    assert!(dst.saved_cursor.is_some());
}

/// v2 regression: feed enough lines to push some into scrollback,
/// roundtrip the snapshot, verify scrollback_len > 0 on the
/// restored terminal AND a couple of cell-level samples match.
/// Without this guard, L3 self-execv silent updates leave panes
/// with empty scrollback (the "大部分窗口没几行历史" user
/// report).
#[test]
fn snapshot_v2_preserves_scrollback_across_roundtrip() {
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut src = Terminal::new(COLS, ROWS);
    // 50 lines of distinct text → 45 pushed into scrollback (ROWS=5).
    for i in 0..50u32 {
        src.feed(format!("line {i:03}\r\n").as_bytes());
    }
    let sb_pre = src.grid().scrollback_len();
    assert!(sb_pre > 0, "test setup expected non-empty scrollback");

    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(COLS, ROWS);
    dst.apply_snapshot(&bytes).unwrap();

    let sb_post = dst.grid().scrollback_len();
    assert_eq!(
        sb_post, sb_pre,
        "scrollback length should survive snapshot roundtrip (was {sb_pre}, got {sb_post})"
    );
    // Spot-check: pick a known-scrollback line and check its first
    // few cells (scrollback_line(0) is the OLDEST scrollback row).
    let newest = dst
        .grid()
        .scrollback_line(0)
        .expect("scrollback line 0 missing after roundtrip");
    let prefix: String = newest.iter().take(8).map(|c| c.ch).collect();
    assert!(
        prefix.starts_with("line "),
        "newest scrollback line should begin with 'line ' prefix; got {prefix:?}"
    );
}

/// F2+4 — when the live scrollback is already non-empty at
/// apply_snapshot time (the common L3 self-execv case: the file-
/// or disk-backed scrollback survived the execv with the full
/// history intact), the snapshot's scrollback section must NOT
/// be replayed.  Replaying duplicates the tail of the persisted
/// scrollback into the file once per execv — measured at
/// ~20 k rows × 9 panes × ~6 installs/day ≈ 1 M dup rows/day
/// before this gate.
#[test]
fn snapshot_v3_fills_execv_gap_without_duplicating_persisted_prefix() {
    // Models the real execv-gap scenario: old L3 had N lines in
    // scrollback, only N-K reached the file before execv (the K
    // tail was buffered in a BufWriter that didn't flush on
    // execv).  Snapshot carries the full N (RAM-ring is truth).
    // New L3 reopens, sees N-K on disk, applies snapshot — must
    // end up with exactly N lines, no duplicates of the on-disk
    // prefix, no missing tail.
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    // Source = old L3 with all N lines.
    let mut src = Terminal::new(COLS, ROWS);
    let mut canonical: Vec<String> = Vec::new();
    for i in 0..50u32 {
        let s = format!("line {i:03}");
        canonical.push(s.clone());
        src.feed(s.as_bytes());
        src.feed(b"\r\n");
    }
    let src_sb_len = src.grid().scrollback_len();
    assert!(src_sb_len > 0);
    let bytes = src.serialize_snapshot();

    // Destination = new L3 reopens scrollback file that has only
    // the persisted PREFIX (first src_sb_len - GAP lines).  We
    // simulate this by feeding dst the same byte sequence for the
    // first `prefix_len` scrollback lines so its scrollback ring
    // ends up containing exactly the bytes the file would have
    // held.
    const GAP: usize = 10;
    let prefix_len = src_sb_len - GAP;
    let mut dst = Terminal::new(COLS, ROWS);
    // Feed enough lines that prefix_len lines land in scrollback.
    // Relation: after feeding N text lines (each followed by
    // \r\n), scrollback length is N - (ROWS - 1).  The first
    // ROWS-1 LFs land cursor at the bottom row WITHOUT scrolling
    // (cursor < scroll_bot until row ROWS-1 is reached); the
    // ROWS-th LF is the first that triggers a scroll-and-push.
    let lines_to_feed = prefix_len + ROWS as usize - 1;
    for s in canonical.iter().take(lines_to_feed) {
        dst.feed(s.as_bytes());
        dst.feed(b"\r\n");
    }
    let dst_pre_apply = dst.grid().scrollback_len();
    assert_eq!(
        dst_pre_apply, prefix_len,
        "test setup: dst should have exactly the persisted prefix in scrollback"
    );

    // Apply snapshot — should fill the GAP, NOT duplicate prefix.
    dst.apply_snapshot(&bytes).unwrap();

    let dst_post = dst.grid().scrollback_len();
    assert_eq!(
        dst_post, src_sb_len,
        "v3 apply should fill exactly the execv gap (expected {src_sb_len}, got {dst_post})"
    );
    // RFC-004 B14 — content-exact check across the WHOLE ring:
    // every line must be the canonical line for its index (the
    // pre-B14 emitter refilled the gap with copies of the OLDEST
    // lines; a prefix-only assertion never noticed).
    for (idx, _) in canonical.iter().enumerate().take(dst_post) {
        let line = dst.grid().scrollback_line(idx).expect("line");
        let txt: String = line
            .iter()
            .take(8)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string();
        assert_eq!(
            txt, canonical[idx],
            "scrollback line {idx} content mismatch after gap fill"
        );
    }
}

/// RFC-004 C.1 (v4) — alt-screen fold.  A terminal inside
/// `?1049h` (claudecode-shaped) serializes the MAIN layer as the
/// snapshot body and appends the alt ring + final alt screen as
/// the v4 fold section; apply lands the alt content in
/// scrollback and restores the main screen.  Pre-v4, this
/// scenario silently lost the whole alt conversation (B13).
#[test]
fn snapshot_v4_folds_alt_screen_history_into_scrollback() {
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut src = Terminal::new(COLS, ROWS);
    // Main-screen prelude: pushes some lines into main scrollback.
    for i in 0..8u32 {
        src.feed(format!("main {i:02}\r\n").as_bytes());
    }
    let main_sb = src.grid().scrollback_len();
    assert!(main_sb > 0);
    // Enter alt (claudecode launch) + emit a conversation that
    // scrolls well past the alt grid.
    src.feed(b"\x1b[?1049h");
    for i in 0..30u32 {
        src.feed(format!("alt line {i:02}\r\n").as_bytes());
    }
    src.feed(b"FINAL ALT SCREEN");
    let bytes = src.serialize_snapshot();

    let mut dst = Terminal::new(COLS, ROWS);
    dst.apply_snapshot(&bytes).unwrap();
    let post = dst.grid().scrollback_len();
    assert!(
        post > main_sb,
        "alt fold must land in scrollback: main={main_sb} post={post}"
    );
    // Newest folded line (idx = len-1; 0 is the OLDEST — B14) =
    // the final visible alt screen content.
    let newest = dst.grid().scrollback_line(post - 1).expect("newest line");
    let text: String = newest.iter().map(|c| c.ch).collect();
    assert!(
        text.starts_with("FINAL ALT SCREEN"),
        "final alt screen row must be the newest history: {text:?}"
    );
    // And the alt conversation lines are present above it.
    let mut found_alt0 = false;
    for idx in 0..post {
        if let Some(l) = dst.grid().scrollback_line(idx) {
            let t: String = l.iter().map(|c| c.ch).collect();
            if t.starts_with("alt line 00") {
                found_alt0 = true;
                break;
            }
        }
    }
    assert!(found_alt0, "oldest alt ring line must survive the fold");
    // The restored VISIBLE grid is the main screen (pre-alt), not
    // the alt shadow — a fresh shell draws over the main prompt.
    let row0: String = (0..COLS).map(|c| dst.grid().cell(c, 0).ch).collect();
    assert!(
        row0.starts_with("main "),
        "visible grid must be the restored main screen: {row0:?}"
    );
    // Mode bookkeeping: restored terminal is NOT in alt mode.
    let bytes2 = dst.serialize_snapshot();
    // Serializing the restored terminal must produce alt_present=0
    // (quick structural probe: v4 payload of a non-alt terminal
    // ends with the alt_present byte = 0).
    assert_eq!(bytes2[bytes2.len() - 1], 0, "restored term must not be alt");
}

/// RFC-004 C.1 amendment (v5) — LIVE snapshot roundtrip for the
/// execv handoff: an alt-screen terminal must come back VERBATIM
/// (current grid = alt, saved_main rebuilt, ring intact, nothing
/// folded into the File scrollback), so the still-running TUI's
/// incremental repaints stay seamless.  The fold form here was
/// the 2026-07-17 "input box vanished after reinstall" field
/// regression.
#[test]
fn snapshot_v5_live_restores_alt_screen_verbatim() {
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut src = Terminal::new(COLS, ROWS);
    for i in 0..8u32 {
        src.feed(format!("main {i:02}\r\n").as_bytes());
    }
    let main_sb = src.grid().scrollback_len();
    src.feed(b"\x1b[?1049h");
    for i in 0..30u32 {
        src.feed(format!("alt line {i:02}\r\n").as_bytes());
    }
    src.feed(b"INPUT BOX ROW");
    let alt_ring = src.grid().scrollback_len();
    let (acc, acr) = src.grid().cursor();
    let bytes = src.serialize_snapshot_live();

    let mut dst = Terminal::new(COLS, ROWS);
    dst.apply_snapshot(&bytes).unwrap();
    // Current screen is the ALT screen, verbatim.
    let last_row: String = (0..COLS).map(|c| dst.grid().cell(c, acr).ch).collect();
    assert!(
        last_row.starts_with("INPUT BOX ROW"),
        "alt screen must be the visible grid: {last_row:?}"
    );
    assert_eq!(dst.grid().cursor(), (acc, acr), "alt cursor verbatim");
    // Alt ring preserved as the CURRENT grid's scrollback…
    assert_eq!(dst.grid().scrollback_len(), alt_ring, "alt ring intact");
    // …and NOT folded into the main layer: leaving alt shows the
    // main screen with its original scrollback length.
    dst.feed(b"\x1b[?1049l");
    assert_eq!(
        dst.grid().scrollback_len(),
        main_sb,
        "main scrollback must not receive folded alt lines"
    );
    let row0: String = (0..COLS).map(|c| dst.grid().cell(c, 0).ch).collect();
    assert!(
        row0.starts_with("main "),
        "exit-alt must land on the restored main screen: {row0:?}"
    );
}

/// RFC-004 C.1 — `serialize_snapshot_capped` with a small tail
/// cap emits only the newest N main-scrollback lines, and apply's
/// v3 index arithmetic still dedupes correctly against a disk
/// that already has everything (periodic-snapshot shape: flush
/// first, then a snapshot whose tail is a sliver).
#[test]
fn snapshot_capped_tail_still_dedupes_on_apply() {
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut src = Terminal::new(COLS, ROWS);
    for i in 0..40u32 {
        src.feed(format!("line {i:03}\r\n").as_bytes());
    }
    let full = src.grid().scrollback_len();
    let bytes = src.serialize_snapshot_capped(4);
    // Destination already holds the FULL history (the flushed-
    // File periodic case).
    let mut dst = Terminal::new(COLS, ROWS);
    for i in 0..40u32 {
        dst.feed(format!("line {i:03}\r\n").as_bytes());
    }
    assert_eq!(dst.grid().scrollback_len(), full);
    dst.apply_snapshot(&bytes).unwrap();
    assert_eq!(
        dst.grid().scrollback_len(),
        full,
        "capped tail must not duplicate lines the disk already has"
    );
}

/// v2 fallback: a synthetic v2 payload arriving at a v3 receiver
/// still applies — the absence of `start_logical_idx` falls back
/// to the F2+4 "skip if disk non-empty" heuristic.  Loses the
/// gap-tail but doesn't crash.  This guards the
/// `snapshot_v >= 3` branch from regressing into "fall through
/// to v3 path for v2 payloads" — that branch would mis-read 8
/// bytes of the line count as a u64 start_idx and corrupt the
/// rest of the parse.
#[test]
fn snapshot_v2_payload_still_apply_able_after_v3_bump() {
    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut src = Terminal::new(COLS, ROWS);
    for i in 0..30u32 {
        src.feed(format!("line {i:03}\r\n").as_bytes());
    }
    let mut bytes = src.serialize_snapshot();
    // Rewrite the version field (bytes [4..8]) from 3 → 2.  Then
    // strip the start_logical_idx field (8 bytes inserted at the
    // start of the trailing scrollback section) so the section
    // matches the v2 layout exactly.  Layout above the
    // scrollback section is version-stable across v2 → v3.
    bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
    // The scrollback section starts at the END of the live grid
    // section.  v3 inserts 8 bytes (start_logical_idx) right at
    // that boundary.  Compute the offset from the wire layout:
    //   4 magic + 4 version + 2 cols + 2 rows + 2 ccol + 2 crow
    // + 2 stop + 2 sbot + 4 modes + 8 generation + ATTRS_BYTES attrs
    // + 1 has_saved_cursor (None → just the flag byte)
    // + (rows*cols * CELL_BYTES) cells
    let header_bytes = 4 + 4 + 2 + 2 + 2 + 2 + 2 + 2 + 4 + 8 + ATTRS_BYTES + 1;
    let cells_bytes = (COLS as usize) * (ROWS as usize) * CELL_BYTES;
    let off = header_bytes + cells_bytes;
    // Splice out the 8-byte start_logical_idx so what remains is
    // valid v2 wire shape.
    let _ = bytes.drain(off..off + 8);

    let mut dst = Terminal::new(COLS, ROWS);
    assert!(
        dst.apply_snapshot(&bytes).is_ok(),
        "synthetic v2 payload must remain apply-able at a v3 receiver"
    );
}

/// v2 wrapped flag survival: feed a URL that overflows the row
/// width so the parser sets wrap, then push it into scrollback,
/// roundtrip the snapshot, and assert the wrapped flag came back.
/// Without this, link-scan after silent update would chop long
/// URLs at the row boundary.
#[test]
fn snapshot_v2_preserves_scrollback_wrapped_flag() {
    const COLS: u16 = 20;
    const ROWS: u16 = 4;
    let mut src = Terminal::new(COLS, ROWS);
    // 40 chars of URL forces wrap at col 20, then 12 \n to push
    // those wrapped rows into scrollback.
    src.feed(b"https://example.com/path/extra/segments/end");
    for _ in 0..12 {
        src.feed(b"\r\n");
    }
    // There should be at least one scrollback row with wrapped=true
    // by now (the row that took the second half of the URL).
    let any_wrapped_pre =
        (0..src.grid().scrollback_len()).any(|i| src.grid().scrollback_wrapped(i));
    assert!(
        any_wrapped_pre,
        "test setup expected a wrapped row in scrollback"
    );

    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(COLS, ROWS);
    dst.apply_snapshot(&bytes).unwrap();

    let any_wrapped_post =
        (0..dst.grid().scrollback_len()).any(|i| dst.grid().scrollback_wrapped(i));
    assert!(
        any_wrapped_post,
        "wrapped flag should survive snapshot roundtrip (lost after execv would break link scans)"
    );
}

/// v2-payload should be apply-able by readers that share the same
/// MIN_COMPAT.  We can't easily fake a v1-image reader here, but
/// we DO verify that a v1-shaped (synthetic) payload still applies
/// via the version-range check — guarding against an accidental
/// `snapshot_v != SNAPSHOT_VERSION` regression that would re-break
/// silent updates when only one side has been bumped.
#[test]
fn snapshot_v1_still_accepted_by_v2_reader() {
    // Build a real v2 snapshot, then rewrite the version field to 1
    // and truncate the trailing scrollback section.  The reader
    // should accept this as a legacy v1 payload and not return an
    // error.
    let mut src = Terminal::new(20, 5);
    src.feed(b"hello world");
    let mut bytes = src.serialize_snapshot();
    // Bytes 4..8 are the version u32 LE — rewrite to 1.
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    // We don't know exactly where the trailing scrollback bytes
    // start without re-doing the parse, but feeding the full body
    // through with `snapshot_v = 1` already short-circuits the
    // trailing parse; trailing bytes are ignored.
    let mut dst = Terminal::new(20, 5);
    dst.apply_snapshot(&bytes)
        .expect("v1-marked payload must still apply under v2 reader");
}

#[test]
fn snapshot_apply_rejects_bad_magic() {
    let mut t = Terminal::new(20, 5);
    let bad = [0u8; 64]; // all zeros — magic is 0xA557_5301
    let err = t.apply_snapshot(&bad).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn snapshot_apply_rejects_truncated_body() {
    let src = term_with(20, 5, b"hello");
    let bytes = src.serialize_snapshot();
    // Truncate to header-only.
    let truncated = &bytes[..30];
    let mut dst = Terminal::new(20, 5);
    let err = dst.apply_snapshot(truncated).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}
