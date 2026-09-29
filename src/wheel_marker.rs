//! Does a screen show the marker a plugin declared?
//!
//! A plugin says "the scroll view is open when this string is on
//! screen" so that L2 can *read* the state instead of remembering it —
//! the program can leave that view on its own, and the key that opens
//! it is usually a toggle, so a remembered flag both goes stale and,
//! once stale, shuts the view the user just asked for.
//!
//! Kept out of `marspot-core` so the same predicate that decides at
//! runtime is the one a probe runs against a live session; a copy in
//! the probe would test the copy.

/// The comparable form of a screen row or a declared marker.
///
/// Whitespace goes, and so does `\0` — the trailing half of a wide
/// char, which is a layout artefact rather than a character.  A
/// program draws headings for humans, not for matchers: codex
/// letter-spaces its rule, so the cells read `/ T R A N S C R I P T /`
/// while the plugin sensibly declares `/TRANSCRIPT/`.
///
/// This cannot invent a match.  A marker is a distinctive run a plugin
/// picked because ordinary output does not contain it; squeezing only
/// asks that its non-blank characters appear in order.
pub fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace() && *c != '\0').collect()
}

/// The squeezed marker, or `None` when it could never match anything.
pub fn needle(marker: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(marker).ok()?;
    let n = squeeze(s);
    (!n.is_empty()).then_some(n)
}

/// Scan a screen for `marker`, reading cells through `cell(col, row)`.
///
/// The accessor is a closure so a `Grid` and a shared-memory snapshot
/// feed the identical scan.  Called once per wheel EVENT, not per
/// tick — a momentum scroll delivers one event carrying many lines.
pub fn shows_marker(
    cols: u16,
    rows: u16,
    cell: impl Fn(u16, u16) -> char,
    marker: &[u8],
) -> bool {
    let Some(needle) = needle(marker) else {
        return false;
    };
    let mut line = String::with_capacity(cols as usize);
    for row in 0..rows {
        line.clear();
        for col in 0..cols {
            line.push(cell(col, row));
        }
        if squeeze(&line).contains(needle.as_str()) {
            return true;
        }
    }
    false
}

/// Should the wheel treat the scroll view as already open?
///
/// `alt_scroll` is the program's own answer — DEC mode 1007, "on this
/// screen the wheel is the arrow keys", which codex sets the instant
/// its transcript opens and clears on the way out.  When a program
/// says it, nothing else gets a vote: a heading read off the screen is
/// an inference, and it lost three times before it was made to work
/// (letter-spacing), while this is a statement.
///
/// The marker stays for programs that never learned to say it.  A
/// plugin that declares no marker gets `true`: without a way to read
/// the state, the only safe answer is "do not send the enter key",
/// since that key is typically a toggle and a blind press would close
/// whatever the user is reading.
///
/// This is the whole rule, kept in one place so a probe replaying it
/// against a live session replays what L2 actually decides.
pub fn view_is_open(
    alt_scroll: bool,
    cols: u16,
    rows: u16,
    cell: impl Fn(u16, u16) -> char,
    marker: &[u8],
) -> bool {
    alt_scroll || marker.is_empty() || shows_marker(cols, rows, cell, marker)
}

/// Does this wheel tick belong to the plugin's scroll view?
///
/// An open view takes both directions — paging back down inside it is
/// how the user returns to the newest line.  A CLOSED view is only
/// ever opened by an UPWARD tick: reaching for history is an upward
/// gesture, while a downward tick at rest means "show me what is
/// below", and answering that by opening a history view is a surprise
/// (asked for 2026-09-06).  A downward tick on a closed view is not
/// ours at all — the pane routes it itself.
pub fn wheel_is_ours(view_open: bool, scrolling_up: bool) -> bool {
    view_open || scrolling_up
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured by `examples/dump_row.rs` off a live codex transcript.
    /// This exact row is what made three wheel fixes look right and
    /// behave wrong.
    const REAL_ROW: &str = "/ T R A N S C R I P T / / / / / / / / / / / / / ";

    fn screen<'a>(rows: &'a [&'a str]) -> impl Fn(u16, u16) -> char + 'a {
        move |col, row| {
            rows.get(row as usize)
                .and_then(|r| r.chars().nth(col as usize))
                .unwrap_or(' ')
        }
    }

    #[test]
    fn letter_spaced_heading_matches_the_compact_marker() {
        assert!(
            !REAL_ROW.contains("/TRANSCRIPT/"),
            "the literal match is the regression; if this ever passes, \
             the screen changed and this test is testing nothing"
        );
        assert!(shows_marker(
            REAL_ROW.chars().count() as u16,
            1,
            screen(&[REAL_ROW]),
            b"/TRANSCRIPT/"
        ));
    }

    #[test]
    fn a_marker_found_on_any_row_counts() {
        let rows = ["shell prompt", "", REAL_ROW, "more output"];
        assert!(shows_marker(60, 4, screen(&rows), b"/TRANSCRIPT/"));
    }

    #[test]
    fn wide_char_trailing_halves_do_not_split_a_marker() {
        assert_eq!(squeeze("/TRAN\0SCRIPT/"), "/TRANSCRIPT/");
    }

    /// The predicate must read "closed" on an ordinary screen, or the
    /// wheel sends a toggle into a program that never asked for one.
    #[test]
    fn ordinary_output_does_not_match() {
        let rows = ["run tests / start / integration", "$ cargo test", ""];
        assert!(!shows_marker(40, 3, screen(&rows), b"/TRANSCRIPT/"));
    }

    /// The wheel opens the view only upward, and once open serves
    /// both directions — otherwise the user could page back up but
    /// never back down.
    #[test]
    fn a_closed_view_is_opened_only_by_scrolling_up() {
        assert!(wheel_is_ours(false, true), "up on a closed view opens it");
        assert!(
            !wheel_is_ours(false, false),
            "down on a closed view is the pane's own scroll, not ours"
        );
        assert!(wheel_is_ours(true, true));
        assert!(
            wheel_is_ours(true, false),
            "an open view must still page down, or there is no way back"
        );
    }

    /// An undeclared marker must never be read as a match — but the
    /// wheel must still treat the view as open, so it does not press a
    /// toggle it cannot observe.
    #[test]
    fn an_undeclared_marker_never_sends_a_blind_toggle() {
        let rows = ["$ cargo test"];
        assert!(!shows_marker(40, 1, screen(&rows), b""));
        assert!(view_is_open(false, 40, 1, screen(&rows), b""));
    }

    /// A program that says "the wheel is the arrow keys here" is
    /// believed, whatever is on the screen.  This is the answer the
    /// marker was always approximating.
    #[test]
    fn the_programs_own_statement_outranks_the_screen() {
        let rows = ["nothing that looks like a transcript"];
        assert!(
            view_is_open(true, 40, 1, screen(&rows), b"/TRANSCRIPT/"),
            "DEC 1007 means the view is open"
        );
        assert!(
            !view_is_open(false, 40, 1, screen(&rows), b"/TRANSCRIPT/"),
            "and without it we are back to reading the screen"
        );
    }

    /// A plugin that declares nothing (or blanks, or non-UTF-8) must
    /// read as "no marker", never as "always open".
    #[test]
    fn unusable_markers_never_match() {
        let rows = [REAL_ROW];
        assert!(!shows_marker(60, 1, screen(&rows), b""));
        assert!(!shows_marker(60, 1, screen(&rows), b"   "));
        assert!(!shows_marker(60, 1, screen(&rows), &[0xff, 0xfe]));
    }
}

/// How long to wait for a just-sent `enter` to show up on screen
/// before concluding it did not take.
///
/// Measured from the user's own log (2026-09-07): sending the key and
/// seeing the program's marker appear was 59 ms apart in the best case
/// observed.  A trackpad delivers wheel events at 60–120 Hz, so during
/// that window four to seven more of them arrive — and each one, seeing
/// a view that still reads closed, sent the toggle AGAIN.  The
/// transcript opened and closed several times inside one flick, which
/// is what "scrolling into history is very choppy" was.
///
/// The state-change log could not see it: it records the OBSERVED
/// open flag, and a flap that resolves before the next publish never
/// changes it.
pub const ENTER_SETTLE_MS: u64 = 400;

/// Should this tick send the `enter` key?
///
/// Deliberately not a remembered "it is open now" bool — the program
/// leaves that view on its own as well as by the user's key, and a
/// stale flag makes the next tick CLOSE what the user is reading (this
/// was tried, and that is what it did).  This asks a narrower question:
/// has enough time passed since we last asked for the answer to be
/// visible?
pub fn should_send_enter(view_open: bool, since_last_enter_ms: Option<u64>) -> bool {
    if view_open {
        return false;
    }
    match since_last_enter_ms {
        Some(ms) => ms >= ENTER_SETTLE_MS,
        None => true,
    }
}

#[cfg(test)]
mod enter_settle_tests {
    use super::*;

    #[test]
    fn an_open_view_is_never_asked_to_open_again() {
        assert!(!should_send_enter(true, None));
        assert!(!should_send_enter(true, Some(10_000)));
    }

    #[test]
    fn the_first_tick_of_a_gesture_asks() {
        assert!(should_send_enter(false, None));
    }

    #[test]
    fn the_ticks_that_arrive_while_it_is_still_answering_do_not() {
        // The flick that produced the report: seven more events inside
        // the window where the marker has not appeared yet.
        for ms in [0, 8, 16, 33, 59, 100, 399] {
            assert!(
                !should_send_enter(false, Some(ms)),
                "{ms} ms after asking, the answer may still be in flight"
            );
        }
    }

    #[test]
    fn a_view_still_closed_long_after_is_asked_again() {
        // The key genuinely did not take — say the program was busy.
        // Waiting forever would leave the wheel dead.
        assert!(should_send_enter(false, Some(ENTER_SETTLE_MS)));
        assert!(should_send_enter(false, Some(5_000)));
    }
}
