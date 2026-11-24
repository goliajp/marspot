//! Whether a key sent to an agent did anything on its screen.
//!
//! What is looked at is what a list or a menu of the agent's moves when
//! it takes the key: the input box, the cursor, how many rows there are
//! under the box, and the first cells of each row from the box down,
//! where an agent marks what is selected.  Text that changes by itself
//! (a footer's words, figures, a spinner) is left out.

use marspot::pane_read::input_box::{box_at, box_of};
use marspot_term::grid::Grid;
use std::hash::{Hash, Hasher};

/// The cells at the start of a row where a selection is marked.
const MARK_COLS: u16 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Look {
    cursor: (u16, u16),
    cursor_shown: bool,
    rows: (u16, u16),
    below: u16,
    marks: u64,
}

/// How a screen looks with an empty input box that has the cursor;
/// None for any other.
pub(super) fn look(screen: &Grid, cursor_shown: bool) -> Option<Look> {
    let at = box_at(screen)?;
    if !box_of(screen)?.is_empty() {
        return None;
    }
    let last = screen.last_used_row().max(at.end);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for r in at.prompt_row..=last.min(screen.rows().saturating_sub(1)) {
        for c in 0..MARK_COLS.min(screen.cols()) {
            let cell = screen.cell(c, r);
            (cell.ch, cell.attrs.fg, cell.attrs.bg, cell.attrs.bold, cell.attrs.reverse, cell.attrs.dim).hash(&mut h);
        }
        let any = |f: fn(&marspot_term::grid::CellAttrs) -> bool| (0..screen.cols()).any(|c| f(&screen.cell(c, r).attrs));
        (any(|a| a.reverse), any(|a| a.bold)).hash(&mut h);
    }
    Some(Look { cursor: screen.cursor(), cursor_shown, rows: (at.prompt_row, at.end), below: last - at.end, marks: h.finish() })
}
