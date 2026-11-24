//! Claude Code's own list of subagents, found on its screen.
//!
//! It is the last thing on the screen, under the input box: a rule, the
//! footer, a blank row, `main`, and a row per subagent.

use super::compose::{Pick, NAMES};
use marspot_term::grid::Grid;

/// Where a list has its columns: how wide the one of names is, and the
/// column after the last one of the figures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in super::super) struct Columns {
    pub names: u16,
    pub right: u16,
}

/// A list that is drawn, with its `main`.
#[derive(Debug, PartialEq)]
pub(in super::super) struct NativeList {
    pub main_row: u16,
    /// The subagents' rows that are drawn.
    pub rows: u16,
    /// The last row that says anything.
    pub last_row: u16,
    /// `None` without a subagent's row.
    pub columns: Option<Columns>,
    pub selected: Option<Pick>,
    /// Whether that is the last row there is.
    pub on_last: bool,
}

/// How far above `main` its input box's rule can be.
const FOOTER: u16 = 8;

fn blank(g: &Grid, col: u16, row: u16) -> bool {
    col >= g.cols() || g.cell(col, row).ch == ' '
}

fn blank_row(g: &Grid, row: u16) -> bool {
    (0..g.cols()).all(|c| blank(g, c, row))
}

fn says(g: &Grid, col: u16, row: u16, text: &str) -> bool {
    text.chars().enumerate().all(|(i, ch)| {
        let col = col + i as u16;
        col < g.cols() && g.cell(col, row).ch == ch
    })
}

/// Whether `row` is laid out as one of the list, and whether it is
/// selected.
fn listed(g: &Grid, row: u16) -> Option<bool> {
    let at = |col| g.cell(col, row).ch;
    let marked = match at(0) {
        '❯' => true,
        ' ' => false,
        _ => return None,
    };
    let dotted = matches!(at(2), '⏺' | '◯') && blank(g, 1, row) && blank(g, 3, row) && !blank(g, 4, row);
    dotted.then_some(marked)
}

/// The list on `g`, if its `main` is drawn.
pub(in super::super) fn find(g: &Grid) -> Option<NativeList> {
    if g.cols() < 10 {
        return None;
    }
    let end = (0..g.rows()).rev().find(|&r| !blank_row(g, r))?;
    let more_below = says(g, 2, end, "↓ ") && blank(g, 0, end);
    let last = end - more_below.then_some(1).filter(|_| end > 0).unwrap_or(0);
    let mut main_row = last;
    loop {
        listed(g, main_row)?;
        if says(g, 4, main_row, "main") && blank(g, 8, main_row) && boxed(g, main_row) {
            break;
        }
        main_row = main_row.checked_sub(1)?;
    }
    let selected = (main_row..=last).find(|&r| listed(g, r) == Some(true));
    Some(NativeList {
        main_row,
        rows: last - main_row,
        last_row: end,
        columns: columns(g, main_row + 1, last),
        selected: selected.map(|r| if r == main_row { Pick::Main } else { Pick::Row((r - main_row - 1) as usize) }),
        on_last: selected == Some(last) && !more_below,
    })
}

fn boxed(g: &Grid, main_row: u16) -> bool {
    main_row > 0
        && blank_row(g, main_row - 1)
        && (main_row.saturating_sub(FOOTER)..main_row).any(|r| g.cell(0, r).ch == '─')
}

/// A row's pieces: what stands between two blanks or more.
fn piece(g: &Grid, row: u16, from: u16) -> Option<(u16, u16)> {
    let start = (from..g.cols()).find(|&c| !blank(g, c, row))?;
    let end = (start..g.cols()).find(|&c| blank(g, c, row) && blank(g, c + 1, row));
    Some((start, end.unwrap_or(g.cols())))
}

/// Whether what starts at `col` is a row's figures.
fn figures(g: &Grid, col: u16, row: u16) -> bool {
    if ["failed", "stopped", "awaiting approval"].iter().any(|t| says(g, col, row, t)) {
        return true;
    }
    let digit = |c: u16| c < g.cols() && g.cell(c, row).ch.is_ascii_digit();
    let end = (col..g.cols()).find(|&c| !digit(c)).unwrap_or(g.cols());
    end > col && end < g.cols() && matches!(g.cell(end, row).ch, 's' | 'm' | 'h' | 'd') && blank(g, end + 1, row)
}

/// The columns of rows `from..=to`.
fn columns(g: &Grid, from: u16, to: u16) -> Option<Columns> {
    let (mut names, mut task_at, mut butted, mut right) = (0, None::<u16>, None::<u16>, 0);
    for row in from..=to {
        let (start, end) = piece(g, row, 4)?;
        right = right.max((start..g.cols()).rev().find(|&c| !blank(g, c, row)).map_or(0, |c| c + 1));
        // with no room for tasks the figures are one blank from the name
        match (start + 1..end).find(|&c| blank(g, c - 1, row) && figures(g, c, row)) {
            Some(c) => butted = Some(c),
            None => names = names.max(end - start),
        }
        match piece(g, row, end) {
            // figures too, when they are cut at the pane's edge
            Some((at, to)) if to < g.cols() && !figures(g, at, row) => task_at = Some(task_at.map_or(at, |t| t.min(at))),
            _ => {}
        }
    }
    let names = match (task_at, butted) {
        (Some(at), _) => at.saturating_sub(6),
        (None, Some(at)) => at - 5,
        (None, None) => names,
    };
    (from <= to).then_some(Columns { names: names.clamp(NAMES.0, NAMES.1), right })
}
