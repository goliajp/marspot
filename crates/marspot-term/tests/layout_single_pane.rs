//! One pane gets no title strip.
//!
//! The strip says which pane is which, and with one pane there is no
//! which — the window header above it already carries the traffic
//! lights and the icon buttons.  Two bands of chrome naming the only
//! thing on screen is what makes a single window look busy next to a
//! plain terminal.
//!
//! This is about the arithmetic, not the decision: given a title
//! height of zero the cell has to hand its whole inner area to the
//! grid, and given a non-zero one it has to reserve it.

use marspot_term::layout::Layout;

fn build(title_h: f64, panes: usize) -> Layout {
    Layout::build(1600.0, 1000.0, 0.0, 56.0, title_h, panes.max(1), 1, 8.0, 16.0)
}

#[test]
fn a_zero_title_gives_its_height_back_to_the_grid() {
    let with_strip = build(48.0, 1);
    let without = build(0.0, 1);

    let rows_with = with_strip.cells[0].rows;
    let rows_without = without.cells[0].rows;
    assert!(
        rows_without > rows_with,
        "dropping the strip has to buy rows: {rows_with} then {rows_without}"
    );
    // 48 physical pixels at a 16 px cell is three rows.
    assert_eq!(rows_without - rows_with, 3);
}

#[test]
fn the_layout_reports_the_height_it_was_given() {
    assert_eq!(build(0.0, 1).cell_title_h, 0.0);
    assert_eq!(build(48.0, 2).cell_title_h, 48.0);
}

/// A zero strip must not eat a row through rounding.
///
/// The cell's inner height is `h - title - 2*padding`, so a zero
/// title has to leave the padding alone rather than folding into it.
#[test]
fn a_zero_strip_does_not_disturb_the_padding() {
    let a = build(0.0, 1);
    let b = build(48.0, 1);
    assert_eq!(a.padding, b.padding, "the padding is not the strip's to change");
    assert!(a.cells[0].rows > b.cells[0].rows);
}
