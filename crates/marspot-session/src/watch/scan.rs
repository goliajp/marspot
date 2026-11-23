//! The rows a text watch is checked on, and the line its text is on.

use marspot_term::grid::Reach;

use crate::SessionImpl;
use crate::track::{At, Batch, cursor_at};
use crate::watch_text::{BATCH_ROWS, Needle, REGISTER_ROWS, Span, find, find_on_screen};

/// Which rows a text watch is checked on.
#[derive(Clone, Copy)]
pub(super) enum Scan {
    /// From where the output after `since` started to the cursor.
    Register,
    /// What a batch wrote; `whole` when its bytes held the watch's text
    /// whole.
    Batch { batch: Batch, whole: bool },
    None,
}

/// The line a text watch's text is on, for the rows `scan` reads;
/// `since` is the watch's position and where output after it started.
pub(super) fn text_line(session: &SessionImpl, n: &Needle, since: (u64, At), scan: Scan, cuts: &mut u32) -> Option<u64> {
    let grid = session.terminal().grid();
    let (since, floor) = since;
    match scan {
        Scan::None => None,
        _ if session.terminal().in_alt_screen() => find_on_screen(grid, n),
        Scan::Register => {
            let at = cursor_at(grid).0;
            find(grid, n, Span { floor, lo: floor.0.min(at), hi: floor.0.max(at) }, REGISTER_ROWS).0
        }
        Scan::Batch { batch, whole } => {
            // whole rows, from where the cursor was or went up to: a
            // carriage return rewrites a row from its start
            let (a, z) = (batch.start(since).0, batch.to.0);
            // the rows it went to and left again hold old text too, so
            // they are read only when the text is known to be new
            let reach = batch.reach.filter(|_| whole).unwrap_or(Reach { top: a, bottom: a });
            let span = Span { floor, lo: a.min(z).min(reach.top), hi: a.max(z).max(reach.bottom) };
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
