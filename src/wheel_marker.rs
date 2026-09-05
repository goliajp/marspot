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
/// A plugin that declares no marker gets `true`: without a way to read
/// the state, the only safe answer is "do not send the enter key",
/// since that key is typically a toggle and a blind press would close
/// whatever the user is reading.
///
/// This is the whole rule, kept in one place so a probe replaying it
/// against a live session replays what L2 actually decides.
pub fn view_is_open(
    cols: u16,
    rows: u16,
    cell: impl Fn(u16, u16) -> char,
    marker: &[u8],
) -> bool {
    marker.is_empty() || shows_marker(cols, rows, cell, marker)
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

    /// An undeclared marker must never be read as a match — but the
    /// wheel must still treat the view as open, so it does not press a
    /// toggle it cannot observe.
    #[test]
    fn an_undeclared_marker_never_sends_a_blind_toggle() {
        let rows = ["$ cargo test"];
        assert!(!shows_marker(40, 1, screen(&rows), b""));
        assert!(view_is_open(40, 1, screen(&rows), b""));
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
