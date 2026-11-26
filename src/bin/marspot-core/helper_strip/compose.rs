//! The rows of a strip as Claude Code draws the list of its own
//! subagents, cell for cell:
//!
//! ```text
//! ❯ ◯ name  task                                 1m 32s · ↓ 115.3k tokens
//! ```
//!
//! The name's column is as wide as the widest name, the figures end at
//! the right edge, and the task has what is left between them.

use super::figures::{cut, figures, width, Text};
use super::native_list::Columns;
use super::strip::{Run, Strip};
use super::theme::Theme;
use marspot_term::grid::CellAttrs;

/// The most rows shown at a time.
pub(in super::super) const WINDOW: usize = 5;
pub(super) const CURSOR: &str = "❯";
pub(super) const IN_VIEW: &str = "⏺";
pub(super) const NOT_IN_VIEW: &str = "◯";
const NAME_COL: u16 = 4;
/// The narrowest and the widest the names' column gets.
pub(super) const NAMES: (u16, u16) = (4, 28);
/// What is blank at the right of a row.
pub(super) const MARGIN: u16 = 2;

#[derive(Clone, Copy, PartialEq)]
pub(in super::super) enum State {
    Running,
    /// Its turn is over and it can be told more.
    Idle,
    /// Its process is gone.
    Done,
    Failed,
    Stopped,
    /// It waits for an approval.
    Awaiting,
    /// It waits for the answer to a question it asked.
    Asked,
}

/// Which way its tokens go: up until it has called a tool.
#[derive(Clone, Copy, PartialEq)]
pub(in super::super) enum Arrow {
    Up,
    Down,
}

#[derive(Clone, Copy)]
pub(in super::super) struct Row<'a> {
    pub name: &'a str,
    /// Whether the name was given to it, rather than said by its kind.
    pub named: bool,
    pub task: &'a str,
    pub state: State,
    pub elapsed_ms: u64,
    pub tokens: Option<u64>,
    pub arrow: Arrow,
}

/// What a strip was made to hold, line by line.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(in super::super) struct Drawn {
    /// The line of the first helper, and which helper that is.
    line: u16,
    first: usize,
    rows: u16,
    more: bool,
}

/// What is on a line of a strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in super::super) enum Spot {
    Main,
    /// A helper, by its place in the list.
    Row(usize),
    More,
}

impl Drawn {
    /// Whether its `main` is the strip's own.
    pub(in super::super) fn has_main(&self) -> bool {
        self.line > 0
    }

    pub(in super::super) fn at(&self, line: u16) -> Option<Spot> {
        let Some(i) = line.checked_sub(self.line) else {
            return (line + 1 == self.line).then_some(Spot::Main);
        };
        match i {
            i if i < self.rows => Some(Spot::Row(self.first + i as usize)),
            i if i == self.rows && self.more => Some(Spot::More),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in super::super) enum Pick {
    Main,
    Row(usize),
}

pub(in super::super) struct List<'a> {
    pub rows: &'a [Row<'a>],
    pub selected: Option<Pick>,
    /// What the pane shows.
    pub viewed: Pick,
    pub theme: &'a Theme,
    /// Where the agent's own list has its columns, when it draws one
    /// with a `main`: the rows go on from it, with no `main` of theirs.
    pub native: Option<Columns>,
}

impl List<'_> {
    /// The first of the `fit` rows in sight.
    fn first(&self, fit: usize) -> usize {
        if fit == 0 {
            return 0;
        }
        let on = match (self.selected, self.viewed) {
            (Some(Pick::Row(i)), _) | (_, Pick::Row(i)) => i,
            _ => 0,
        };
        (on + 1).saturating_sub(fit).min(self.rows.len().saturating_sub(fit))
    }

    /// The first row in sight, and how many are, with `room` lines for
    /// them and for the one that says how many more there are.
    fn window(&self, room: usize) -> (usize, usize) {
        let fit = WINDOW.min(room);
        let first = self.first(fit);
        if first + fit < self.rows.len() && room == fit {
            let fit = fit.saturating_sub(1);
            return (self.first(fit), fit);
        }
        (first, fit)
    }
}

/// The lines above the rows: a blank one and `main`, unless the rows go
/// on from the agent's own list.  A strip too short for both and its
/// rows gives up the blank line first, then `main`.
fn head(rows: u16, list: &List) -> u16 {
    if list.native.is_some() {
        return 0;
    }
    let n = list.rows.len();
    let listed = n.min(WINDOW) as u16 + u16::from(n > WINDOW);
    rows.saturating_sub(listed).min(2)
}

/// Write `list` into `strip`, under its head.  The same rows again
/// allocate nothing.
pub(in super::super) fn compose(strip: &mut Strip, list: &List) -> Drawn {
    let line = head(strip.rows(), list);
    let (first, fit) = list.window(strip.rows().saturating_sub(line) as usize);
    if line == 2 {
        strip.line(0, &[]);
    }
    if line > 0 {
        main_row(strip, list, first, line - 1);
    }
    let place = Place::of(strip.cols(), list);
    let mut at = line;
    for (i, row) in list.rows.iter().enumerate().skip(first).take(fit) {
        place.row(strip, at, list, i, row);
        at += 1;
    }
    let rows = at - line;
    let below = list.rows.len().saturating_sub(first + fit);
    if below > 0 {
        let more = more("↓", below);
        strip.line(at, &[Run { col: 2, text: more.as_str(), attrs: list.theme.dim() }]);
        at += 1;
    }
    for rest in at..strip.rows() {
        strip.line(rest, &[]);
    }
    Drawn { line, first, rows, more: below > 0 }
}

fn more(arrow: &str, n: usize) -> Text {
    use std::fmt::Write;
    let mut t = Text::EMPTY;
    let _ = write!(t, "{arrow} {n} more");
    t
}

fn main_row(strip: &mut Strip, list: &List, above: usize, at: u16) {
    let (selected, viewed) = (list.selected == Some(Pick::Main), list.viewed == Pick::Main);
    let attrs = match (selected, viewed) {
        (true, _) => list.theme.selected(viewed),
        (false, true) => Theme::plain(true),
        (false, false) => list.theme.dim(),
    };
    let dot = if viewed { IN_VIEW } else { NOT_IN_VIEW };
    let up = if above > 0 { more("↑", above) } else { Text::EMPTY };
    let up_col = strip.cols().saturating_sub(MARGIN + width(up.as_str()));
    strip.line(
        at,
        &[
            cursor(list.theme, selected),
            Run { col: 2, text: dot, attrs },
            Run { col: NAME_COL, text: "main", attrs },
            Run { col: up_col.max(NAME_COL + 5), text: up.as_str(), attrs: list.theme.dim() },
        ],
    );
}

fn cursor(theme: &Theme, selected: bool) -> Run<'static> {
    Run { col: 0, text: if selected { CURSOR } else { "" }, attrs: theme.selected(false) }
}

/// Where the columns of a strip's rows are.
struct Place {
    cols: u16,
    names: u16,
    /// Where the figures end, and how wide their column is at least.
    right: u16,
    figures: u16,
}

impl Place {
    fn of(cols: u16, list: &List) -> Self {
        let names = list.rows.iter().map(|r| width(r.name)).max().unwrap_or(0).clamp(NAMES.0, NAMES.1);
        let figures = list.rows.iter().map(|r| width(figures(r, false).as_str())).max().unwrap_or(0);
        let own = Columns { names, right: cols.saturating_sub(MARGIN) };
        let Columns { names, right } = list.native.unwrap_or(own);
        Self { cols, names, right, figures }
    }

    fn row(&self, strip: &mut Strip, at: u16, list: &List, i: usize, row: &Row) {
        let picked = list.selected == Some(Pick::Row(i));
        let viewed = list.viewed == Pick::Row(i);
        let ink = Ink::of(list.theme, row, picked, viewed);
        let figures = figures(row, true);
        let room = self.figures.max(width(figures.as_str()));
        // the name gives up nothing: in a narrow pane the figures stay
        // right of it and are cut at the pane's edge
        let left = self.right.saturating_sub(room).max(NAME_COL + self.names + 1);
        let figures_at = left + room - width(figures.as_str());
        let task_at = (NAME_COL + self.names + 2).min(left);
        let (name, name_end) = cut(row.name, self.names);
        let (task, task_end) = cut(row.task, left.saturating_sub(task_at + 1));
        let (figures, figures_end) = cut(figures.as_str(), self.cols.saturating_sub(figures_at));
        strip.line(
            at,
            &[
                cursor(list.theme, picked),
                Run { col: 2, text: if viewed { IN_VIEW } else { NOT_IN_VIEW }, attrs: ink.dot },
                Run { col: NAME_COL, text: name, attrs: ink.name },
                Run { col: 0, text: name_end, attrs: ink.name },
                Run { col: task_at, text: task, attrs: ink.task },
                Run { col: 0, text: task_end, attrs: ink.task },
                Run { col: figures_at, text: figures, attrs: ink.figures },
                Run { col: 0, text: figures_end, attrs: ink.figures },
            ],
        );
    }
}

struct Ink {
    dot: CellAttrs,
    name: CellAttrs,
    task: CellAttrs,
    figures: CellAttrs,
}

impl Ink {
    fn of(theme: &Theme, row: &Row, picked: bool, viewed: bool) -> Self {
        let quiet = if picked || viewed { Theme::plain(viewed) } else { theme.dim() };
        let task = if picked { theme.selected(viewed) } else { quiet };
        let dot = match row.state {
            State::Done => theme.success(viewed),
            State::Failed | State::Stopped => theme.error(viewed),
            State::Running | State::Awaiting | State::Asked | State::Idle => task,
        };
        let name = if row.named && !picked { Theme::plain(viewed) } else { task };
        Self { dot, name, task, figures: quiet }
    }
}
