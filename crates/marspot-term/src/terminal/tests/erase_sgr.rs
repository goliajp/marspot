//! Erase in display and line, SGR, and the blanks erase and scroll leave.

use super::*;

#[test]
fn ed_zero_erases_from_cursor_to_end_of_screen() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    let before = count_marker(&t, '#'); // 50
    t.feed(b"\x1B[J"); // ED default = 0
    let cleared = before - count_marker(&t, '#');
    // From (3, 2) inclusive to end of screen: row 2 has cols 3..10 = 7,
    // rows 3 and 4 each have 10, total = 7 + 20 = 27.
    assert_eq!(cleared, 27);
    // Cursor unchanged.
    assert_eq!(t.grid().cursor(), (3, 2));
    // Cells before cursor untouched.
    assert_eq!(t.grid().cell(2, 2).ch, '#');
    assert_eq!(t.grid().cell(3, 2).ch, ' ');
    assert_eq!(t.grid().cell(0, 4).ch, ' ');
}

#[test]
fn ed_one_erases_from_start_to_cursor_inclusive() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[1J");
    let remaining = count_marker(&t, '#');
    // Row 2 cols 0..=3 cleared (4 cells), rows 0 and 1 cleared (20).
    // Total cleared: 24.  Remaining: 50 - 24 = 26.
    assert_eq!(remaining, 26);
    assert_eq!(t.grid().cursor(), (3, 2));
    assert_eq!(t.grid().cell(3, 2).ch, ' ', "cursor cell must be cleared");
    assert_eq!(t.grid().cell(4, 2).ch, '#', "cell after cursor must remain");
}

#[test]
fn ed_two_erases_entire_screen() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[2J");
    assert_eq!(count_marker(&t, '#'), 0);
    assert_eq!(t.grid().cursor(), (3, 2), "cursor must not move");
}

#[test]
fn el_zero_erases_from_cursor_to_end_of_line() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[K");
    // Row 2 cols 3..10 = 7 cleared.
    assert_eq!(count_marker(&t, '#'), 50 - 7);
    assert_eq!(t.grid().cursor(), (3, 2));
    assert_eq!(t.grid().cell(2, 2).ch, '#');
    assert_eq!(t.grid().cell(9, 2).ch, ' ');
    // Other rows untouched.
    assert_eq!(t.grid().cell(5, 1).ch, '#');
    assert_eq!(t.grid().cell(5, 3).ch, '#');
}

#[test]
fn el_one_erases_from_start_of_line_to_cursor_inclusive() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[1K");
    // Row 2 cols 0..=3 = 4 cleared.
    assert_eq!(count_marker(&t, '#'), 50 - 4);
    assert_eq!(t.grid().cell(3, 2).ch, ' ');
    assert_eq!(t.grid().cell(4, 2).ch, '#');
    // Other rows untouched.
    assert_eq!(t.grid().cell(0, 1).ch, '#');
}

#[test]
fn el_two_erases_entire_line() {
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[2K");
    // Row 2 fully cleared (10 cells).
    assert_eq!(count_marker(&t, '#'), 50 - 10);
    assert_eq!(t.grid().cursor(), (3, 2));
    // Other rows untouched.
    assert_eq!(t.grid().cell(0, 1).ch, '#');
    assert_eq!(t.grid().cell(0, 3).ch, '#');
}

#[test]
fn ed_three_does_not_panic_or_modify_screen() {
    // ED 3 erases scrollback in xterm; we have no scrollback yet, so it
    // must be a no-op (not panic, not clear visible screen).
    let mut t = filled_terminal(10, 5, '#', 3, 2);
    t.feed(b"\x1B[3J");
    assert_eq!(count_marker(&t, '#'), 50);
}

// ----- SGR (Select Graphic Rendition) -----

#[test]
fn sgr_empty_is_full_reset() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[1;31;42m");
    assert!(t.current_attrs().bold);
    assert_eq!(t.current_attrs().fg, Color::indexed(1));
    t.feed(b"\x1B[m");
    assert_eq!(t.current_attrs(), CellAttrs::default());
}

#[test]
fn sgr_zero_resets_all() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[1;31;42m");
    t.feed(b"\x1B[0m");
    assert_eq!(t.current_attrs(), CellAttrs::default());
}

#[test]
fn sgr_bold_on_off_independent_of_color() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[31m\x1B[1m");
    assert!(t.current_attrs().bold);
    assert_eq!(t.current_attrs().fg, Color::indexed(1));
    t.feed(b"\x1B[22m"); // un-bold; fg stays
    assert!(!t.current_attrs().bold);
    assert_eq!(t.current_attrs().fg, Color::indexed(1));
}

#[test]
fn sgr_individual_attribute_toggles() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[3m\x1B[4m\x1B[7m");
    let a = t.current_attrs();
    assert!(a.italic && a.underline && a.reverse);
    t.feed(b"\x1B[23m\x1B[24m\x1B[27m");
    let a = t.current_attrs();
    assert!(!a.italic && !a.underline && !a.reverse);
}

#[test]
fn sgr_standard_8_color_fg() {
    for (code, idx) in (30u16..=37).zip(0u8..=7) {
        let mut t = Terminal::new(10, 5);
        t.feed(format!("\x1B[{}m", code).as_bytes());
        assert_eq!(
            t.current_attrs().fg,
            Color::indexed(idx),
            "code {} -> idx {}",
            code,
            idx
        );
    }
}

#[test]
fn sgr_standard_8_color_bg() {
    for (code, idx) in (40u16..=47).zip(0u8..=7) {
        let mut t = Terminal::new(10, 5);
        t.feed(format!("\x1B[{}m", code).as_bytes());
        assert_eq!(
            t.current_attrs().bg,
            Color::indexed(idx),
            "code {} -> idx {}",
            code,
            idx
        );
    }
}

#[test]
fn sgr_bright_8_color() {
    // 90–97 → indices 8–15 (bright fg); 100–107 → bright bg.
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[91m"); // bright red fg
    assert_eq!(t.current_attrs().fg, Color::indexed(9));
    t.feed(b"\x1B[105m"); // bright magenta bg
    assert_eq!(t.current_attrs().bg, Color::indexed(13));
}

#[test]
fn sgr_default_fg_and_bg() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[31;41m\x1B[39;49m");
    assert_eq!(t.current_attrs().fg, Color::DEFAULT);
    assert_eq!(t.current_attrs().bg, Color::DEFAULT);
}

#[test]
fn sgr_256_color_fg() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[38;5;208m");
    assert_eq!(t.current_attrs().fg, Color::indexed(208));
}

#[test]
fn sgr_256_color_bg() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[48;5;42m");
    assert_eq!(t.current_attrs().bg, Color::indexed(42));
}

#[test]
fn sgr_truecolor_fg() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[38;2;100;200;50m");
    assert_eq!(t.current_attrs().fg, Color::rgb(100, 200, 50));
}

#[test]
fn sgr_truecolor_bg() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[48;2;10;20;30m");
    assert_eq!(t.current_attrs().bg, Color::rgb(10, 20, 30));
}

#[test]
fn sgr_combined_in_single_sequence() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[1;31;42m");
    let a = t.current_attrs();
    assert!(a.bold);
    assert_eq!(a.fg, Color::indexed(1));
    assert_eq!(a.bg, Color::indexed(2));
}

#[test]
fn sgr_unknown_code_is_skipped_not_panicked() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[1;999;31m"); // 999 unknown
    let a = t.current_attrs();
    assert!(a.bold);
    assert_eq!(a.fg, Color::indexed(1)); // 31 still applied after 999 skipped
}

#[test]
fn printed_cell_carries_current_attrs() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"\x1B[1;31mA");
    let cell = t.grid().cell(0, 0);
    assert_eq!(cell.ch, 'A');
    assert!(cell.attrs.bold);
    assert_eq!(cell.attrs.fg, Color::indexed(1));
}

#[test]
fn already_printed_cells_unaffected_by_later_sgr() {
    let mut t = Terminal::new(10, 5);
    t.feed(b"A\x1B[31mB");
    // 'A' was printed before SGR — must keep default attrs.
    assert_eq!(t.grid().cell(0, 0).attrs.fg, Color::DEFAULT);
    // 'B' was printed after — must have red fg.
    assert_eq!(t.grid().cell(1, 0).attrs.fg, Color::indexed(1));
}

/// 2026-07-29 field report — the omz update banner grew stray
/// horizontal rules through its links: a `\e[4m` URL wrapping on
/// the bottom row scrolled a fresh row in while underline was
/// still on, and the blanks it was filled with carried the
/// underline.  BCE inherits ONLY the background; every other SGR
/// bit is a glyph property and blanks have no glyph.
#[test]
fn scroll_fill_and_erase_blanks_carry_only_the_background() {
    // Scroll-fill: bottom-row wrap mid-underline.
    let mut t = Terminal::new(20, 2);
    t.feed(b"x\r\ny");
    t.feed(b"\r\x1b[41m\x1b[4m0123456789abcdefghijKLM\x1b[24m\x1b[0m");
    let g = t.grid();
    // Row 1 is the wrapped continuation: "KLM" + fill blanks.
    for c in 3..g.cols() {
        let cell = g.cell(c, 1);
        assert!(
            !cell.attrs.underline,
            "fill blank at col {c} must not be underlined"
        );
        assert!(!cell.attrs.bold && !cell.attrs.reverse && !cell.attrs.dim);
        assert_eq!(cell.attrs.bg, Color::indexed(1), "…but BCE keeps the bg");
    }
    // The printed glyphs DO keep their underline.
    assert!(
        g.cell(0, 1).attrs.underline,
        "the K is genuinely underlined"
    );

    // Erase path (EL) mid-underline: same contract.
    let mut t = Terminal::new(10, 2);
    t.feed(b"\x1b[42m\x1b[4mab\x1b[K");
    let g = t.grid();
    assert!(g.cell(0, 0).attrs.underline && g.cell(1, 0).attrs.underline);
    for c in 2..10 {
        let cell = g.cell(c, 0);
        assert!(
            !cell.attrs.underline,
            "EL blank at col {c} must not be underlined"
        );
        assert_eq!(cell.attrs.bg, Color::indexed(2));
    }
}

#[test]
fn bce_erase_fills_with_current_attrs() {
    // Background Color Erase: ED/EL fills cleared cells with the
    // current SGR attrs, not default.  Critical for vim/tmux which
    // paint full-screen backgrounds via CSI 41 m + CSI 2 J.
    let mut t = Terminal::new(10, 3);
    t.feed(b"\x1B[41m\x1B[2J"); // bg red + clear screen
    for r in 0..3 {
        for c in 0..10 {
            let cell = t.grid().cell(c, r);
            assert_eq!(cell.ch, ' ');
            assert_eq!(cell.attrs.bg, Color::indexed(1), "cell ({},{}) bg", c, r);
        }
    }
}

#[test]
fn bce_erase_in_line_uses_current_bg() {
    let mut t = Terminal::new(10, 3);
    t.feed(b"\x1B[42m\x1B[K"); // bg green + EL 0
    for c in 0..10 {
        assert_eq!(t.grid().cell(c, 0).attrs.bg, Color::indexed(2));
    }
    // Other rows untouched.
    assert_eq!(t.grid().cell(0, 1).attrs.bg, Color::DEFAULT);
}
