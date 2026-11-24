//! One pane's strip: the rows it asks for, what they hold, and where
//! they are drawn.

use marspot::render::StripView;
use marspot_term::grapheme::{cluster_width, graphemes};
use marspot_term::grid::{Cell, CellAttrs, Grid};
use std::time::{Duration, Instant};

/// The most rows a strip may take.
pub(in super::super) const MAX_ROWS: u16 = 8;

/// How long what is above it must have stayed shorter before it moves
/// up: a screen being redrawn is short for a moment.
const UP_AFTER: Duration = Duration::from_millis(150);

/// Text at a column of a strip's row.
pub(in super::super) struct Run<'a> {
    pub col: u16,
    pub text: &'a str,
    pub attrs: CellAttrs,
}

pub(in super::super) struct Strip {
    grid: Grid,
    /// Changes whenever a cell of `grid` does.
    seq: u64,
    rows: u16,
    /// Where it is drawn, in rows from the terminal's first; `None`
    /// while it is not.
    top_row: Option<u16>,
    /// The last used row of a screen, by what it was worked out from:
    /// the session shown, its frame and its height.
    last_used: Option<((u64, u64, u16), u16)>,
    /// The session it was last placed under.
    shown: Option<u64>,
    up: Option<Up>,
}

/// A move up that waits: where to, until when, and whether the frame
/// that makes it has been asked for.
struct Up {
    to: u16,
    due: Instant,
    asked: bool,
}

/// What a strip is placed under.
pub(in super::super) struct Over<'a> {
    pub screen: &'a Grid,
    /// The session `screen` is of, and its frame.
    pub shown: u64,
    pub seq: u64,
    pub scrolled: bool,
    /// The terminal's height in the layout.
    pub rows: u16,
    /// The last row what is being composed takes.
    pub preedit_end: Option<u16>,
    pub now: Instant,
}

/// The clusters of `text` that take cells, and how many each takes.
/// Control characters take none.
pub(in super::super) fn glyphs(text: &str) -> impl Iterator<Item = (&str, u16)> {
    graphemes(text).filter_map(|g| {
        let first = g.chars().next()?;
        // a selector with nothing before it to select for
        if first.is_control() || matches!(first, '\u{fe0e}' | '\u{fe0f}') {
            return None;
        }
        Some((g, cluster_width(g) as u16)).filter(|(_, w)| *w > 0)
    })
}

/// Where a new strip's `seq` starts, so that one that takes another's
/// place is never taken for it.
fn first_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1 << 40, std::sync::atomic::Ordering::Relaxed)
}

impl Strip {
    pub(super) fn new(rows: u16) -> Self {
        Self {
            grid: Grid::with_scrollback(1, rows, 0),
            seq: first_seq(),
            rows,
            top_row: None,
            last_used: None,
            shown: None,
            up: None,
        }
    }

    /// The rows it asks the layout for.
    pub(in super::super) fn rows(&self) -> u16 {
        self.rows
    }

    pub(in super::super) fn top_row(&self) -> Option<u16> {
        self.top_row
    }

    pub(super) fn set_rows(&mut self, rows: u16) {
        self.rows = rows;
    }

    /// Take the size the layout gave, and say whether that changed it:
    /// what it held is gone then.
    pub(super) fn fit(&mut self, cols: u16, rows: u16) -> bool {
        if (self.grid.cols(), self.grid.rows()) == (cols, rows) {
            return false;
        }
        self.grid = Grid::with_scrollback(cols, rows, 0);
        self.seq += 1;
        true
    }

    /// Make row `row` hold exactly `runs`, which are in column order.
    /// Only cells that differ are written, and nothing is allocated.
    pub(in super::super) fn line(&mut self, row: u16, runs: &[Run]) {
        if row >= self.grid.rows() {
            return;
        }
        let cols = self.grid.cols();
        let mut col = 0;
        for run in runs {
            while col < run.col.min(cols) {
                self.put(col, row, Cell::default());
                col += 1;
            }
            for (g, width) in glyphs(run.text) {
                if col + width > cols {
                    break;
                }
                self.put_glyph(col, row, g, run.attrs);
                if width == 2 {
                    self.put(col + 1, row, Cell { ch: '\0', attrs: run.attrs });
                }
                col += width;
            }
        }
        while col < cols {
            self.put(col, row, Cell::default());
            col += 1;
        }
        // entries of cells since overwritten
        if self.grid.cluster_pool_len() > cols as usize * self.grid.rows() as usize {
            self.grid.sweep_clusters();
        }
    }

    fn put_glyph(&mut self, col: u16, row: u16, glyph: &str, attrs: CellAttrs) {
        let mut chars = glyph.chars();
        match (chars.next(), chars.next()) {
            (Some(ch), None) => self.put(col, row, Cell { ch, attrs }),
            _ => self.seq += self.grid.put_cluster(col, row, glyph, attrs) as u64,
        }
    }

    fn put(&mut self, col: u16, row: u16, cell: Cell) {
        if self.grid.cell(col, row) != cell {
            self.grid.set_cell(col, row, cell);
            self.seq += 1;
        }
    }

    /// Work out where it goes: right under what the screen holds, or
    /// under its last row while it shows its history.  A screen still
    /// taller than the layout says has rows where the strip goes, and
    /// the strip waits.  It moves down at once and up after `UP_AFTER`.
    pub(in super::super) fn place(&mut self, over: Over) {
        let want = if over.screen.rows() > over.rows {
            None
        } else if over.scrolled {
            Some(over.rows)
        } else {
            let end = self.last_used_row(over.screen, over.shown, over.seq);
            Some((end.max(over.preedit_end.unwrap_or(0)) + 1).min(over.rows))
        };
        let same = self.shown.replace(over.shown) == Some(over.shown);
        let at = self.top_row.map(|t| t.min(over.rows));
        self.top_row = match (want, at) {
            (Some(want), Some(at)) if same && want < at => Some(self.up_to(want, at, over.now)),
            _ => {
                self.up = None;
                want
            }
        };
    }

    /// Where it is while it waits to go from `at` up to `want`.
    fn up_to(&mut self, want: u16, at: u16, now: Instant) -> u16 {
        match &self.up {
            Some(up) if up.to == want && up.due <= now => {
                self.up = None;
                want
            }
            Some(up) if up.to == want => at,
            _ => {
                self.up = Some(Up { to: want, due: now + UP_AFTER, asked: false });
                at
            }
        }
    }

    /// When a frame is owed for it to move up.
    pub(super) fn due(&self) -> Option<Instant> {
        self.up.as_ref().filter(|u| !u.asked).map(|u| u.due)
    }

    /// Whether that frame is owed now.  True once.
    pub(super) fn ripen(&mut self, now: Instant) -> bool {
        match &mut self.up {
            Some(up) if !up.asked && up.due <= now => {
                up.asked = true;
                true
            }
            _ => false,
        }
    }

    fn last_used_row(&mut self, screen: &Grid, shown: u64, seq: u64) -> u16 {
        let of = (shown, seq, screen.rows());
        match self.last_used {
            Some((was, row)) if was == of => row,
            _ => {
                let row = screen.last_used_row();
                self.last_used = Some((of, row));
                row
            }
        }
    }

    pub(super) fn hide(&mut self) {
        self.top_row = None;
        self.up = None;
    }

    pub(super) fn view(&self) -> Option<StripView<'_>> {
        Some(StripView { grid: &self.grid, seq: self.seq, top_row: self.top_row? })
    }

    #[cfg(test)]
    pub(super) fn grid(&self) -> &Grid {
        &self.grid
    }

    #[cfg(test)]
    pub(super) fn seq(&self) -> u64 {
        self.seq
    }
}
