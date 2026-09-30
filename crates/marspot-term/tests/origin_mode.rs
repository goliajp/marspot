//! DECOM: rows counted from the top margin instead of the top of the
//! screen.
//!
//! A program that sets a scroll region and then draws inside it is
//! describing a window, and with origin mode on it can go on calling
//! the first row of that window row 1. Without the mode implemented,
//! every such program drew its window at the top of the screen instead
//! of where it had asked for it -- which is what vttest's origin mode
//! page reports when it says a line should be at the bottom and it is
//! not.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

/// A ten-row screen with rows 5..8 (1-based) as the region.
fn with_region() -> Terminal {
    let mut t = Terminal::new(12, 10);
    t.feed(b"\x1b[5;8r");
    t
}

#[test]
fn row_one_is_the_top_of_the_region() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[1;1Htop");
    assert_eq!(row(&t, 4), "top", "row 1 means the region's first row");
    assert_eq!(row(&t, 0), "", "and nothing was drawn at the screen's");
}

#[test]
fn without_the_mode_row_one_is_the_screen() {
    let mut t = with_region();
    t.feed(b"\x1b[1;1Htop");
    assert_eq!(row(&t, 0), "top");
}

#[test]
fn the_cursor_cannot_leave_the_region() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[99;1Hfar");
    assert_eq!(row(&t, 7), "far", "clamped to the region's last row");
}

#[test]
fn vpa_counts_from_the_same_place_as_cup() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[2dx");
    assert_eq!(row(&t, 5), "x");
}

#[test]
fn turning_the_mode_on_or_off_homes_the_cursor() {
    let mut t = with_region();
    t.feed(b"\x1b[9;9H");
    t.feed(b"\x1b[?6h");
    assert_eq!(t.grid().cursor(), (0, 4), "home is the region's first row");
    t.feed(b"\x1b[?6l");
    assert_eq!(t.grid().cursor(), (0, 0), "and the screen's, once it is off");
}

#[test]
fn setting_a_region_homes_into_it() {
    let mut t = Terminal::new(12, 10);
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[3;6r");
    assert_eq!(t.grid().cursor(), (0, 2));
}

/// A program asking where it is has to be answered in the rows it
/// counts in, or it moves relative to a number that meant something
/// else.
#[test]
fn the_cursor_report_uses_the_same_origin() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[2;3H");
    t.feed(b"\x1b[6n");
    assert_eq!(t.take_response(), b"\x1b[2;3R");

    // Turning the mode off homes the cursor, so ask again from a row
    // named the other way.
    t.feed(b"\x1b[?6l");
    t.feed(b"\x1b[6;3H");
    t.feed(b"\x1b[6n");
    assert_eq!(t.take_response(), b"\x1b[6;3R", "off, it is the screen row");
}

#[test]
fn the_terminal_reports_the_mode_it_is_actually_in() {
    let mut t = with_region();
    t.feed(b"\x1b[?6$p");
    assert_eq!(t.take_response(), b"\x1b[?6;2$y", "reset");
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[?6$p");
    assert_eq!(t.take_response(), b"\x1b[?6;1$y", "set");
}

#[test]
fn a_saved_cursor_carries_the_mode() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b7"); // saved with origin mode on
    t.feed(b"\x1b[?6l");
    t.feed(b"\x1b8");
    t.feed(b"\x1b[1;1Hback");
    assert_eq!(row(&t, 4), "back", "the restore brought the addressing back too");
}

#[test]
fn a_reset_turns_it_off() {
    let mut t = with_region();
    t.feed(b"\x1b[?6h");
    t.feed(b"\x1b[!p"); // DECSTR
    t.feed(b"\x1b[1;1Htop");
    assert_eq!(row(&t, 0), "top");
}
