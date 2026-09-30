//! OSC 133: where one command's territory ends and the next begins.
//!
//! A shell that emits these turns a wall of output into a sequence of
//! commands. Without them a terminal cannot tell a prompt from any
//! other line that happens to start with a glyph, so "go back to the
//! command before this one" has nothing to go to.
//!
//! What is *not* here: the marks are not written to disk. The
//! scrollback record has one byte for `wrapped` and its reader decides
//! on `!= 0`, so a spare bit in it would read as a wrapped line to a
//! binary that predates this -- silently, and only when reflowing.
//! Persisting waits for the sidecar in the line-identity design.

use marspot_term::grid::PromptMark;
use marspot_term::terminal::Terminal;

fn osc(body: &str) -> Vec<u8> {
    format!("\x1b]{body}\x07").into_bytes()
}

#[test]
fn the_four_boundaries_land_on_the_row_the_cursor_is_on() {
    let mut t = Terminal::new(20, 6);
    for (row, body, want) in [
        (0u16, "133;A", PromptMark::PromptStart),
        (1, "133;B", PromptMark::InputStart),
        (2, "133;C", PromptMark::OutputStart),
        (3, "133;D;0", PromptMark::CommandEnd(Some(0))),
    ] {
        t.feed(format!("\x1b[{};1H", row + 1).as_bytes());
        t.feed(&osc(body));
        assert_eq!(t.grid().row_prompt(row), want, "{body}");
    }
}

/// A shell that did not say how the command exited is not a shell
/// that said it succeeded.
#[test]
fn no_status_is_not_a_status_of_zero() {
    let mut t = Terminal::new(20, 6);
    t.feed(&osc("133;D"));
    assert_eq!(t.grid().row_prompt(0), PromptMark::CommandEnd(None));

    t.feed(b"\x1b[2;1H");
    t.feed(&osc("133;D;0"));
    assert_eq!(t.grid().row_prompt(1), PromptMark::CommandEnd(Some(0)));

    t.feed(b"\x1b[3;1H");
    t.feed(&osc("133;D;1"));
    assert_eq!(t.grid().row_prompt(2), PromptMark::CommandEnd(Some(1)));
}

/// Options ride along on every letter and are not what is being
/// reported. `D`'s first parameter is the exception -- it is the
/// status -- so an option there must not be read as one.
#[test]
fn options_are_not_mistaken_for_a_status() {
    let mut t = Terminal::new(20, 6);
    t.feed(&osc("133;A;aid=14;cl=line"));
    assert_eq!(t.grid().row_prompt(0), PromptMark::PromptStart);

    t.feed(b"\x1b[2;1H");
    t.feed(&osc("133;D;aid=14"));
    assert_eq!(
        t.grid().row_prompt(1),
        PromptMark::CommandEnd(None),
        "aid=14 is not an exit status"
    );
}

#[test]
fn a_letter_we_do_not_know_changes_nothing() {
    let mut t = Terminal::new(20, 6);
    t.feed(&osc("133;A"));
    t.feed(&osc("133;Q"));
    assert_eq!(t.grid().row_prompt(0), PromptMark::PromptStart);
}

/// The mark belongs to the line, so it has to go into history with it.
#[test]
fn a_mark_follows_its_line_into_the_scrollback() {
    let mut t = Terminal::new(20, 3);
    t.feed(&osc("133;A"));
    t.feed(b"prompt\r\n");
    for _ in 0..6 {
        t.feed(b"output\r\n");
    }
    let sb = t.grid().scrollback_len();
    assert!(sb >= 4, "the fixture has to actually push lines out, got {sb}");
    let marked: Vec<usize> = (0..sb)
        .filter(|i| t.grid().scrollback_prompt(*i) == PromptMark::PromptStart)
        .collect();
    assert_eq!(marked, vec![0], "exactly the first line, and it is in history");
}

#[test]
fn there_is_nowhere_to_jump_before_the_first_prompt() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"just output\r\n");
    assert_eq!(t.grid().prompt_above(0), None);
}

/// The point of the whole thing: from the bottom, go to where the
/// last command started.
#[test]
fn jumping_back_lands_on_the_prompt() {
    let mut t = Terminal::new(20, 4);
    t.feed(&osc("133;A"));
    t.feed(b"first prompt\r\n");
    for _ in 0..10 {
        t.feed(b"output\r\n");
    }
    let off = t.grid().prompt_above(0).expect("there is a prompt behind us");
    assert!(off > 0, "it has to be somewhere above the live screen");

    // The line that offset puts at the top is the marked one.
    let sb = t.grid().scrollback_len();
    let top_abs = sb + t.grid().rows() as usize - t.grid().rows() as usize - off as usize;
    assert_eq!(
        t.grid().scrollback_prompt(top_abs),
        PromptMark::PromptStart,
        "the offset points at a line that is not the prompt"
    );
}
