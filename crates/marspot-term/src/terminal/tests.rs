mod lanes;
mod snapshot;
mod scrollback_page;
mod csi;
mod erase_sgr;
mod scroll_alt_predict;
mod reflow;

use super::*;

fn term_with(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
    let mut t = Terminal::new(cols, rows);
    t.feed(bytes);
    t
}

// ─── RFC-002 step 2 snapshot tests ───────────────────────────────

/// Helper: deep-equal grid cells + cursor + the bits apply_snapshot
/// actually restores.  Lets snapshot tests assert "after apply, the
/// terminal is observationally indistinguishable from the source"
/// without manually comparing every field.
fn assert_terms_equivalent(a: &Terminal, b: &Terminal) {
    let ga = a.grid();
    let gb = b.grid();
    assert_eq!(ga.cols(), gb.cols(), "cols mismatch");
    assert_eq!(ga.rows(), gb.rows(), "rows mismatch");
    assert_eq!(ga.cursor(), gb.cursor(), "cursor mismatch");
    assert_eq!(a.scroll_top, b.scroll_top, "scroll_top");
    assert_eq!(a.scroll_bot, b.scroll_bot, "scroll_bot");
    assert_eq!(
        a.cursor_key_application_mode, b.cursor_key_application_mode,
        "DECCKM"
    );
    assert_eq!(
        a.bracketed_paste_mode, b.bracketed_paste_mode,
        "bracketed paste"
    );
    assert_eq!(a.cursor_visible, b.cursor_visible, "cursor visible");
    assert_eq!(a.pending_wrap, b.pending_wrap, "pending_wrap");
    assert_eq!(a.attrs, b.attrs, "current SGR attrs");
    assert_eq!(
        a.saved_cursor.map(|s| (s.col, s.row, s.attrs)),
        b.saved_cursor.map(|s| (s.col, s.row, s.attrs)),
        "saved cursor"
    );
    assert_eq!(a.generation, b.generation, "generation");
    for r in 0..ga.rows() {
        for c in 0..ga.cols() {
            let ca = ga.cell(c, r);
            let cb = gb.cell(c, r);
            assert_eq!(ca, cb, "cell mismatch at ({},{})", c, r);
        }
    }
}

/// Fill the grid with a marker char at every cell so erase regions are
/// visible by absence.  Returns a Terminal with cursor parked at the
/// requested (col, row) and all cells populated.
fn filled_terminal(cols: u16, rows: u16, marker: char, cur_col: u16, cur_row: u16) -> Terminal {
    let mut t = Terminal::new(cols, rows);
    // Manually fill: write `marker` cols times per row, no parsing.
    // We use direct grid access only in tests — production code goes
    // through Terminal::feed.
    for r in 0..rows {
        for c in 0..cols {
            t.grid.set_cell(
                c,
                r,
                Cell {
                    ch: marker,
                    ..Default::default()
                },
            );
        }
    }
    t.grid.set_cursor(cur_col, cur_row);
    t
}

fn count_marker(t: &Terminal, marker: char) -> usize {
    let g = t.grid();
    let mut n = 0;
    for r in 0..g.rows() {
        for c in 0..g.cols() {
            if g.cell(c, r).ch == marker {
                n += 1;
            }
        }
    }
    n
}

// ----- alt screen (?1049) ---------------------------------------------

fn first_row_text(t: &Terminal) -> String {
    let g = t.grid();
    (0..g.cols()).map(|c| g.cell(c, 0).ch).collect()
}

fn row_text(t: &Terminal, r: u16) -> String {
    let g = t.grid();
    let mut s: String = (0..g.cols())
        .map(|c| g.cell(c, r).ch)
        .filter(|&ch| ch != '\0')
        .collect();
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

/// All logical content — scrollback then live rows, glued by the
/// continuation flags, trailing blank lines dropped.  This is the
/// width-independent invariant: it must survive any resize chain.
fn logical_text(t: &Terminal) -> String {
    let g = t.grid();
    let mut lines: Vec<String> = Vec::new();
    let absorb = |row: String, wrapped: bool, lines: &mut Vec<String>| {
        let row = row.trim_end().to_string();
        if wrapped && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&row);
        } else {
            lines.push(row);
        }
    };
    for i in 0..g.scrollback_len() {
        let row: String = g
            .scrollback_line(i)
            .unwrap()
            .iter()
            .map(|c| c.ch)
            .filter(|&ch| ch != '\0')
            .collect();
        absorb(row, g.scrollback_wrapped(i), &mut lines);
    }
    for r in 0..g.rows() {
        let row: String = (0..g.cols())
            .map(|c| g.cell(c, r).ch)
            .filter(|&ch| ch != '\0')
            .collect();
        absorb(row, g.row_wrapped(r), &mut lines);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
