//! Snapshot generations and scrollback pages.

use super::*;

#[test]
fn feed_bumps_generation_only_on_non_empty_input() {
    let mut t = Terminal::new(20, 5);
    assert_eq!(t.generation(), 0);
    t.feed(b""); // no-op
    assert_eq!(t.generation(), 0, "empty feed must not bump");
    t.feed(b"x");
    assert_eq!(t.generation(), 1);
    t.feed(b"yz");
    assert_eq!(t.generation(), 2, "each non-empty feed bumps by 1");
}

#[test]
fn snapshot_then_more_input_advances_generation_past_loaded_value() {
    // Generation is observational: after apply, additional feed
    // bumps past the loaded value.  Critical for RFC-002 LWW
    // convergence (the client must keep moving forward).
    let src = term_with(20, 5, b"hello");
    let loaded_gen = src.generation();
    assert!(loaded_gen >= 1);
    let bytes = src.serialize_snapshot();
    let mut dst = Terminal::new(20, 5);
    dst.apply_snapshot(&bytes).unwrap();
    assert_eq!(dst.generation(), loaded_gen);
    dst.feed(b"world");
    assert_eq!(dst.generation(), loaded_gen + 1);
}

#[test]
fn scrollback_page_roundtrip_recovers_lines() {
    // 8-col / 2-row grid + \r\n between single-char lines avoids
    // wrap weirdness: each line is one cell + spaces, scroll pushes
    // the top row into scrollback whole.  Feed A..F (6 lines) →
    // grid keeps the last two (E, F), scrollback holds A,B,C,D.
    let mut t = Terminal::new(8, 2);
    t.feed(b"\x1b[31mA\r\nB\r\nC\r\nD\r\nE\r\nF");
    let total = t.grid().scrollback_len();
    assert_eq!(total, 4);
    let (line_count, body) = t.serialize_scrollback_page(0, total as u32);
    assert_eq!(line_count, 4);
    let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
    assert_eq!(lines.len(), 4);
    // Read_lines is oldest-first: A,B,C,D.
    assert_eq!(lines[0].0[0].ch, 'A');
    assert_eq!(lines[3].0[0].ch, 'D');
    // Foreground colour survives the wire format.
    assert_eq!(lines[0].0[0].attrs.fg, Color::indexed(1));
    // Each line padded to grid width.
    assert_eq!(lines[0].0.len(), 8);
    // Hard newlines, not autowrap continuations.
    assert!(!lines[0].1);
}

/// A page that claims more cells than it can hold is an error,
/// not an abort.
///
/// `line_cols` comes off the wire -- another process wrote the
/// page, and it is read back from disk -- so it is a claim. The
/// decoder used to hand it straight to `Vec::with_capacity`, and a
/// claim of a few hundred million cells is an allocation failure,
/// which aborts the process rather than failing the read.
///
/// It took a Linux CI runner to show it: the legacy probe below
/// mis-parses by one byte deliberately, so the second line's
/// length is whatever the cell bytes spell, and on a dev box with
/// 62 GB the allocation can still succeed where a container's
/// limits say no. Same code, same input, different answer -- the
/// kind only another machine asks.
#[test]
fn a_page_claiming_more_than_it_holds_is_an_error() {
    // One line, claiming 300 million cells, with nothing after it.
    let mut body = Vec::new();
    body.extend_from_slice(&300_000_000u32.to_le_bytes());
    body.push(0); // wrapped
    let err = Terminal::decode_scrollback_page_body(1, &body)
        .expect_err("a length the body cannot back is not a length");
    // *Which* error matters. Without the check this still fails --
    // it allocates for 300 million cells first and then runs out
    // of bytes to read -- and a test that only asks `is_err()`
    // goes green on the version that aborts. It has to say the
    // refusal happened before the allocation.
    assert!(
        err.to_string().contains("line claims"),
        "must be refused before allocating, not after; got: {err}"
    );

    // And the honest version of the same shape still decodes.
    let attrs = CellAttrs::default();
    let mut ok = Vec::new();
    ok.extend_from_slice(&1u32.to_le_bytes());
    ok.push(0);
    ok.extend_from_slice(&('Z' as u32).to_le_bytes());
    ok.extend_from_slice(&serialize_attrs(attrs));
    let lines = Terminal::decode_scrollback_page_body(1, &ok).expect("a page that adds up");
    assert_eq!(lines[0].0[0].ch, 'Z');
}

#[test]
fn decode_scrollback_page_legacy_no_wrapped_byte() {
    // L4 shelld 0.2.6 and earlier emit each line as
    // `[cols u32][cells]` with no wrapped byte — decoder must
    // detect that and fall back, reporting wrapped=false for
    // every row so cells line up correctly.
    let attrs = CellAttrs::default();
    let mkcell = |ch: char| Cell { ch, attrs };

    // Hand-build a legacy 2-line, 3-col body.
    let mut body = Vec::new();
    for line_ch in ['X', 'Y'] {
        body.extend_from_slice(&3u32.to_le_bytes());
        for _ in 0..3 {
            body.extend_from_slice(&(line_ch as u32).to_le_bytes());
            body.extend_from_slice(&serialize_attrs(mkcell(line_ch).attrs));
        }
    }
    let lines = Terminal::decode_scrollback_page_body(2, &body).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].0[0].ch, 'X');
    assert_eq!(lines[1].0[2].ch, 'Y');
    assert!(!lines[0].1);
    assert!(!lines[1].1);
}

#[test]
fn scrollback_page_count_overrun_clamps_to_available() {
    let mut t = Terminal::new(8, 2);
    t.feed(b"A\r\nB\r\nC\r\nD\r\nE\r\nF");
    let (line_count, body) = t.serialize_scrollback_page(0, 100);
    assert_eq!(line_count, 4);
    let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
    assert_eq!(lines.len(), 4);
}

#[test]
fn push_historic_line_fills_scrollback_in_caller_order() {
    // Empty scrollback → push 3 historic lines oldest-first →
    // scrollback_line at idx 0..3 returns them in the same order,
    // and a subsequent live-data scroll-up appends after them.
    let mut t = Terminal::new(8, 2);
    let attrs = CellAttrs::default();
    let mkline = |ch: char| -> Vec<Cell> { (0..8).map(|_| Cell { ch, attrs }).collect() };
    t.push_historic_line(&mkline('H'), false);
    t.push_historic_line(&mkline('I'), false);
    t.push_historic_line(&mkline('J'), false);
    assert_eq!(t.grid().scrollback_len(), 3);
    assert_eq!(t.grid().scrollback_line(0).unwrap()[0].ch, 'H');
    assert_eq!(t.grid().scrollback_line(2).unwrap()[0].ch, 'J');
    // Live data continues from where historic ended.
    t.feed(b"K\r\nL\r\nM\r\nN");
    // 4 new lines, grid rows=2, so 2 of them spill into scrollback
    // (oldest-first append) → scrollback now [H, I, J, K, L].
    assert_eq!(t.grid().scrollback_len(), 5);
    assert_eq!(t.grid().scrollback_line(3).unwrap()[0].ch, 'K');
    assert_eq!(t.grid().scrollback_line(4).unwrap()[0].ch, 'L');
}

#[test]
fn scrollback_page_empty_scrollback_returns_zero_count() {
    let t = Terminal::new(80, 24);
    let (line_count, body) = t.serialize_scrollback_page(0, 50);
    assert_eq!(line_count, 0);
    assert!(body.is_empty());
    let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
    assert!(lines.is_empty());
}

#[test]
fn snapshot_size_is_proportional_to_grid_area() {
    // 80×24 grid ≈ 80*24*13 = 24960 + ~50 header.
    let src = Terminal::new(80, 24);
    let bytes = src.serialize_snapshot();
    let cells_part = 80 * 24 * CELL_BYTES;
    assert!(
        bytes.len() >= cells_part && bytes.len() <= cells_part + 256,
        "wire size {} not within [cells={}, +256]",
        bytes.len(),
        cells_part
    );
}
