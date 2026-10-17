//! Tabs, through the parser, on a real grid.
//!
//! The unit tests in `tabs` prove the arithmetic.  These prove the
//! wiring: that a tab byte reaches it, that the escape sequences move
//! the stops, and that text lands where a program laying out columns
//! with tabs expects it to.

use marspot_term::terminal::Terminal;

fn row(t: &Terminal, r: u16) -> String {
    let cols = t.grid().cols();
    (0..cols).map(|c| t.grid().cell(c, r).ch).collect::<String>().trim_end().to_string()
}

#[test]
fn a_tab_moves_to_the_next_stop() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"a\tb\tc");
    // Stops at 8, 16, 24: `a` at 0, `b` at 8, `c` at 16.
    assert_eq!(row(&t, 0), "a       b       c");
}

#[test]
fn a_tab_used_to_be_dropped() {
    // The regression this exists for.  If a tab ever stops moving the
    // cursor again, these two columns collapse into `ab` and every
    // program that lays out with tabs comes out mangled.
    let mut t = Terminal::new(40, 4);
    t.feed(b"a\tb");
    assert_ne!(row(&t, 0), "ab", "the tab was swallowed");
    assert_eq!(t.grid().cursor().0, 9, "cursor sits after the b at column 8");
}

#[test]
fn a_program_can_set_and_clear_stops() {
    let mut t = Terminal::new(40, 4);
    // Clear every stop, put one at column 3, then tab to it.
    t.feed(b"\x1b[3g");          // TBC 3 — clear every stop
    t.feed(b"\x1b[4G");          // HPA to column 4 (1-indexed) = col 3
    t.feed(b"\x1bH");            // HTS — a stop here
    t.feed(b"\x1b[1G");          // back to the left margin
    t.feed(b"x\ty");
    assert_eq!(row(&t, 0), "x  y", "the stop a program set is where the tab went");
}

#[test]
fn forward_and_backward_tabs() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b[3I");           // CHT 3 → column 24
    assert_eq!(t.grid().cursor().0, 24);
    t.feed(b"\x1b[2Z");           // CBT 2 → column 8
    assert_eq!(t.grid().cursor().0, 8);
}

#[test]
fn a_tab_at_the_right_margin_stays_put() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"\x1b[3I");           // past the last stop → column 19
    assert_eq!(t.grid().cursor().0, 19);
    t.feed(b"\t");
    assert_eq!(t.grid().cursor().0, 19, "a tab at the margin does not wrap");
}
