//! The rows a text watch is checked on, and the line its text is on.

use crate::SessionImpl;
use crate::track::{At, Batch, cursor_at};
use crate::watch_text::{BATCH_ROWS, Needle, REGISTER_ROWS, Span, find, find_on_screen};

/// Which rows a text watch is checked on.
#[derive(Clone, Copy)]
pub(super) enum Scan {
    /// From where the output after `since` started to the cursor.
    Register,
    /// What a batch wrote.
    Batch(Batch),
    None,
}

/// The line a text watch's text is on, for the rows `scan` reads.
pub(super) fn text_line(session: &SessionImpl, n: &Needle, from: At, scan: Scan, cuts: &mut u32) -> Option<u64> {
    let grid = session.terminal().grid();
    match scan {
        Scan::None => None,
        _ if session.terminal().in_alt_screen() => find_on_screen(grid, n),
        Scan::Register => {
            let at = cursor_at(grid).0;
            find(grid, n, Span { floor: from, lo: from.0.min(at), hi: from.0.max(at) }, REGISTER_ROWS).0
        }
        Scan::Batch(Batch { from: a, to: z }) => {
            // whole rows, from where the cursor was or went up to: a
            // carriage return rewrites a row from its start
            let span = Span { floor: from, lo: a.0.min(z.0), hi: a.0.max(z.0) };
            let (line, cut) = find(grid, n, span, BATCH_ROWS);
            *cuts += u32::from(cut);
            line
        }
    }
}

/// The cursor ended the batch above the screen's last row: a line break
/// there lands on a row that may still hold old text, which the byte
/// stream cannot see, so the grid is read.
pub(super) fn above_bottom(session: &SessionImpl) -> bool {
    let grid = session.terminal().grid();
    grid.cursor().1 + 1 < grid.rows()
}
