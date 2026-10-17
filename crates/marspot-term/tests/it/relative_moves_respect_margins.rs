//! A relative cursor move stops at the margin.
//!
//! CUU and CUD were clamping to the screen instead of to the scroll
//! region, so `CSI 24 B` inside a two-row region put the cursor five
//! rows below the region and everything written afterwards landed
//! outside it -- vttest's scroll page leaves five rows of debris under
//! its own screen that way.
//!
//! The rule is the DEC one, and it is not "clamp to the region": a
//! cursor that is already outside stops at the edge of the screen
//! instead. Only a cursor inside the region is held by the margin.

use marspot_term::terminal::Terminal;

/// Ten rows, region 4..=6 (1-based), so there is room on both sides.
fn regioned() -> Terminal {
    let mut t = Terminal::new(8, 10);
    t.feed(b"\x1b[4;7r");
    t
}

fn row_of(t: &Terminal) -> u16 {
    t.grid().cursor().1
}

#[test]
fn cursor_down_stops_at_the_bottom_margin() {
    let mut t = regioned();
    t.feed(b"\x1b[4;1H"); // inside, at the top of the region
    t.feed(b"\x1b[24B");
    assert_eq!(row_of(&t), 6, "the region's last row, not the screen's");
}

#[test]
fn cursor_up_stops_at_the_top_margin() {
    let mut t = regioned();
    t.feed(b"\x1b[7;1H");
    t.feed(b"\x1b[24A");
    assert_eq!(row_of(&t), 3);
}

#[test]
fn a_cursor_already_below_the_region_runs_to_the_bottom_of_the_screen() {
    let mut t = regioned();
    t.feed(b"\x1b[9;1H"); // below the region
    t.feed(b"\x1b[24B");
    assert_eq!(row_of(&t), 9, "not pulled back up to the margin");
}

#[test]
fn a_cursor_already_above_the_region_runs_to_the_top_of_the_screen() {
    let mut t = regioned();
    t.feed(b"\x1b[2;1H");
    t.feed(b"\x1b[24A");
    assert_eq!(row_of(&t), 0);
}

#[test]
fn the_line_moves_follow_the_same_margins() {
    let mut t = regioned();
    t.feed(b"\x1b[4;5H\x1b[24E"); // CNL
    assert_eq!(t.grid().cursor(), (0, 6));
    t.feed(b"\x1b[7;5H\x1b[24F"); // CPL
    assert_eq!(t.grid().cursor(), (0, 3));
}

#[test]
fn without_a_region_they_run_to_the_screen_edges() {
    let mut t = Terminal::new(8, 10);
    t.feed(b"\x1b[5;1H\x1b[24B");
    assert_eq!(row_of(&t), 9);
    t.feed(b"\x1b[24A");
    assert_eq!(row_of(&t), 0);
}

/// The debris this fixes: text written after an over-long CUD has to
/// stay inside the region.
#[test]
fn text_after_an_over_long_move_stays_in_the_region() {
    let mut t = regioned();
    t.feed(b"\x1b[4;1H\x1b[24Bhere");
    let below: String = (7..10)
        .flat_map(|r| (0..8).map(move |c| (r, c)))
        .map(|(r, c)| t.grid().cell(c, r).ch)
        .collect();
    assert_eq!(below.trim_matches(|c: char| c == ' ' || c == '\0'), "");
}
