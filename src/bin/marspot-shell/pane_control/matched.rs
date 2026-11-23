//! The line a text wait matched, by the absolute row the session
//! reported, though it may have scrolled off the screen since.
//!
//! The session numbers rows from the grid's scroll count, which its
//! published frame carries.  The record's tail is replayed into a
//! headless terminal of the pane's size; its last screen is the live
//! one, so the two numberings differ by the difference of their scroll
//! counts, and the replay holds the history the frame does not.

use marspot_term::grid::Grid;

/// The most screen rows one logical line is read across.
const MAX_WRAPPED_ROWS: u64 = 256;

/// The logical line (soft-wrapped rows joined) that starts at absolute
/// row `top` of pane `sid`; `None` when the record no longer reaches it.
pub(super) fn line_at(sid: u64, top: u64) -> Option<String> {
    let entry = marspot_term::session_registry::read_session_entry(sid).ok()?;
    let regions = marspot_term::grid_shm::SessionRegions::of_entry(&entry)?;
    let live = marspot_term::grid_shm::peek_frame(&regions).ok().flatten()?;
    let term = marspot::pane_read::replayed(sid, live.cols, live.rows).ok()?;
    let grid = term.grid();
    let shift = live.scroll_push_count.checked_sub(grid.scroll_push_count())?;
    logical_line(grid, top.checked_sub(shift)?)
}

/// Where absolute row `abs` of `grid` is, as `(view_offset, row)`.
fn at_view(grid: &Grid, abs: u64) -> Option<(u16, u16)> {
    let rows = u64::from(grid.rows());
    let bottom = grid.scroll_push_count() + rows - 1;
    let up = bottom.checked_sub(abs)?;
    if up < rows {
        return Some((0, (rows - 1 - up) as u16));
    }
    let off = up - (rows - 1);
    (off as usize <= grid.scrollback_len() && off <= u64::from(u16::MAX)).then_some((off as u16, 0))
}

/// The logical line starting at absolute row `abs` of `grid`.
pub(super) fn logical_line(grid: &Grid, abs: u64) -> Option<String> {
    let mut out = String::new();
    let (mut off, mut row) = at_view(grid, abs)?;
    for n in 1..=MAX_WRAPPED_ROWS {
        push_row(grid, off, row, &mut out);
        let Some(next) = at_view(grid, abs + n) else { break };
        if !grid.wrapped_at_view(next.0, next.1) {
            break;
        }
        (off, row) = next;
    }
    Some(out.trim_end().to_string())
}

fn push_row(grid: &Grid, off: u16, row: u16, out: &mut String) {
    let mut clusters = Vec::new();
    grid.row_clusters_at_view(off, row, &mut clusters);
    for col in 0..grid.cols() {
        let cell = grid.cell_at_view(off, col, row);
        // a wide glyph's second cell
        if cell.ch == '\0' {
            continue;
        }
        match grid.cluster_text(&cell).or_else(|| marspot_term::grid::cluster_in_row(&clusters, col)) {
            Some(t) => out.push_str(t),
            None => out.push(cell.ch),
        }
    }
}
