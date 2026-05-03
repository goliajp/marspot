//! Terminal cell grid + bounded scrollback.
//!
//! `Grid` is a passive container: a rectangle of `Cell`s, a cursor position,
//! and a ring of scrolled-off lines.  It exposes primitive operations
//! (`set_cell`, `set_cursor`, `scroll_up`) and lets the emulator layer
//! (`terminal::Handler`) implement VT semantics on top of them.
//!
//! Scrollback is a ring buffer with a fixed capacity allocated at
//! construction.  This is **load-bearing** for the project's "cannot get
//! slower over time" commitment: terminal output is unbounded, but our
//! memory cost is not.  Once the ring is full, oldest lines are evicted
//! O(1).  Disk-backed scrollback (truly unlimited history) is a later
//! phase; it will live behind the same `Grid` interface.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', attrs: CellAttrs::default() }
    }
}

impl From<char> for Cell {
    fn from(ch: char) -> Self {
        Cell { ch, attrs: CellAttrs::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    /// "Use the default foreground/background" — the renderer picks the
    /// theme's default.  This is distinct from Indexed(0), which is the
    /// palette's first color.
    #[default]
    Default,
    /// Indexed palette entry.  0–7 = standard, 8–15 = bright variants,
    /// 16–255 = 256-color extended palette.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

/// Default scrollback capacity — 10 000 lines.  At 80 cols × ~12 bytes/cell
/// this is ~9 MB per terminal pre-allocated.  Tunable via
/// [`Grid::with_scrollback`].
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

pub struct Grid {
    cols: u16,
    rows: u16,
    /// Row-major: index = row * cols + col.
    cells: Vec<Cell>,
    /// Cursor as (col, row).  Always bounded to [0, cols-1] x [0, rows-1].
    cursor_col: u16,
    cursor_row: u16,
    scrollback: ScrollbackRing,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES)
    }

    pub fn with_scrollback(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        let cells = vec![Cell::default(); cols as usize * rows as usize];
        Self {
            cols,
            rows,
            cells,
            cursor_col: 0,
            cursor_row: 0,
            scrollback: ScrollbackRing::new(scrollback_lines, cols as usize),
        }
    }

    pub fn cols(&self) -> u16 { self.cols }
    pub fn rows(&self) -> u16 { self.rows }
    pub fn cursor(&self) -> (u16, u16) { (self.cursor_col, self.cursor_row) }

    pub fn cell(&self, col: u16, row: u16) -> Cell {
        debug_assert!(col < self.cols && row < self.rows);
        self.cells[row as usize * self.cols as usize + col as usize]
    }

    /// Clamp-and-set the cursor.  Out-of-bounds values clamp to the last
    /// valid position; the cursor is always within `[0, cols) x [0, rows)`.
    pub fn set_cursor(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.cursor_row = row.min(self.rows - 1);
    }

    /// Overwrite a single cell.  Caller is responsible for valid coords.
    pub fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        debug_assert!(col < self.cols && row < self.rows);
        self.cells[row as usize * self.cols as usize + col as usize] = cell;
    }

    /// Scroll the visible region up by `lines`.  The displaced top rows are
    /// pushed into scrollback (in order, oldest first) and the bottom
    /// `lines` rows are filled with `fill` — pass a Cell carrying the
    /// current SGR background to honor BCE.  The cursor is NOT moved.
    pub fn scroll_up(&mut self, lines: u16, fill: Cell) {
        if lines == 0 {
            return;
        }
        let lines = lines.min(self.rows) as usize;
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        // 1) Push displaced top rows into scrollback in chronological order.
        for r in 0..lines {
            let start = r * cols;
            self.scrollback.push_line(&self.cells[start..start + cols]);
        }
        // 2) Shift the rest up.  copy_within handles the overlapping ranges.
        if lines < rows {
            let shift = rows - lines;
            let src = lines * cols;
            let len = shift * cols;
            self.cells.copy_within(src..src + len, 0);
        }
        // 3) Fill the bottom `lines` rows with `fill`.
        let blank_start = (rows - lines) * cols;
        for c in &mut self.cells[blank_start..] {
            *c = fill;
        }
    }

    pub fn scrollback_len(&self) -> usize { self.scrollback.len }
    pub fn scrollback_capacity(&self) -> usize { self.scrollback.capacity }
    pub fn scrollback_line(&self, idx: usize) -> Option<&[Cell]> { self.scrollback.line(idx) }
    pub fn clear_scrollback(&mut self) { self.scrollback.clear(); }

    /// Resize the visible grid. Cells in the overlap region are preserved
    /// (top-left anchored); new area is filled with default cells; rows or
    /// columns that fall outside the new size are dropped.  Cursor clamps
    /// into bounds.  Scrollback is reset because its rows are stored at the
    /// old column width — proper reflow is a later refinement.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        let new_total = cols as usize * rows as usize;
        let mut new_cells = vec![Cell::default(); new_total];
        let copy_rows = self.rows.min(rows) as usize;
        let copy_cols = self.cols.min(cols) as usize;
        let old_cols = self.cols as usize;
        let new_cols = cols as usize;
        for r in 0..copy_rows {
            let old_off = r * old_cols;
            let new_off = r * new_cols;
            new_cells[new_off..new_off + copy_cols]
                .copy_from_slice(&self.cells[old_off..old_off + copy_cols]);
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        if self.cursor_col >= cols {
            self.cursor_col = cols - 1;
        }
        if self.cursor_row >= rows {
            self.cursor_row = rows - 1;
        }
        let cap = self.scrollback.capacity;
        self.scrollback = ScrollbackRing::new(cap, cols as usize);
    }
}

/// Bounded ring of scrolled-off lines.  Memory is allocated once at
/// construction (`capacity_lines * cols * size_of::<Cell>()`) and never
/// grows; once full, `push_line` evicts the oldest in O(1).
struct ScrollbackRing {
    /// Flat backing buffer: `capacity * cols` cells when capacity > 0.
    cells: Vec<Cell>,
    /// Number of lines this ring can hold.  0 disables scrollback entirely.
    capacity: usize,
    cols: usize,
    /// Index (in lines) of the oldest entry within `cells`.
    head: usize,
    /// Number of valid lines currently stored.  `len <= capacity`.
    len: usize,
}

impl ScrollbackRing {
    fn new(capacity: usize, cols: usize) -> Self {
        let cells = if capacity == 0 || cols == 0 {
            Vec::new()
        } else {
            vec![Cell::default(); capacity * cols]
        };
        Self { cells, capacity, cols, head: 0, len: 0 }
    }

    fn push_line(&mut self, source: &[Cell]) {
        if self.capacity == 0 {
            return;
        }
        debug_assert_eq!(source.len(), self.cols, "pushed line width mismatches scrollback cols");

        let target = if self.len < self.capacity {
            let line = (self.head + self.len) % self.capacity;
            self.len += 1;
            line
        } else {
            // Capacity reached: overwrite oldest, advance head.
            let line = self.head;
            self.head = (self.head + 1) % self.capacity;
            line
        };
        let start = target * self.cols;
        self.cells[start..start + self.cols].copy_from_slice(source);
    }

    fn line(&self, idx: usize) -> Option<&[Cell]> {
        if idx >= self.len {
            return None;
        }
        let line = (self.head + idx) % self.capacity;
        let start = line * self.cols;
        Some(&self.cells[start..start + self.cols])
    }

    fn clear(&mut self) {
        // Don't zero the backing memory — `len = 0` makes existing data
        // invisible and the next push will overwrite cleanly.  This keeps
        // clear() O(1) regardless of capacity.
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_grid_is_all_default_cells_with_origin_cursor() {
        let g = Grid::new(80, 24);
        assert_eq!(g.cols(), 80);
        assert_eq!(g.rows(), 24);
        assert_eq!(g.cursor(), (0, 0));
        for r in 0..24 {
            for c in 0..80 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn set_cursor_clamps_to_grid_bounds() {
        let mut g = Grid::new(10, 5);
        g.set_cursor(99, 99);
        assert_eq!(g.cursor(), (9, 4));
    }

    #[test]
    fn scroll_up_one_pushes_top_row_to_scrollback() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'a'.into());
        g.set_cell(1, 0, 'b'.into());
        g.set_cell(2, 0, 'c'.into());
        g.set_cell(0, 1, 'd'.into());
        g.set_cell(1, 1, 'e'.into());
        g.set_cell(2, 1, 'f'.into());

        g.scroll_up(1, Cell::default());

        // Row 0 became scrollback line 0, row 1 moved up to row 0, row 1 blank.
        let sb = g.scrollback_line(0).expect("scrollback[0] should exist");
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        assert_eq!(g.cell(0, 0).ch, 'd');
        assert_eq!(g.cell(2, 0).ch, 'f');
        assert_eq!(g.cell(0, 1), Cell::default());
        assert_eq!(g.scrollback_len(), 1);
    }

    #[test]
    fn scroll_up_uses_fill_cell_for_blank_bottom() {
        // BCE: caller passes a stamped Cell; the new bottom rows must adopt it.
        let mut g = Grid::new(3, 3);
        let blank = Cell {
            ch: ' ',
            attrs: CellAttrs { bg: Color::Indexed(1), ..CellAttrs::default() },
        };
        g.scroll_up(2, blank);
        for c in 0..3 {
            for r in 1..3 {
                assert_eq!(g.cell(c, r), blank);
            }
        }
    }

    #[test]
    fn scroll_up_more_than_height_clears_screen_and_pushes_all() {
        let mut g = Grid::new(2, 2);
        g.set_cell(0, 0, 'A'.into());
        g.set_cell(0, 1, 'B'.into());
        g.scroll_up(5, Cell::default()); // scroll farther than height
        // All 2 rows pushed (capped at rows).
        assert_eq!(g.scrollback_len(), 2);
        // Visible region is fully blank.
        for r in 0..2 {
            for c in 0..2 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
    }

    #[test]
    fn scrollback_evicts_oldest_at_capacity() {
        let mut g = Grid::with_scrollback(2, 2, 3); // capacity 3 lines
        // Fill row 0 with 'X', row 1 blank, scroll once → scrollback[0] = "XX".
        // Repeat with markers to verify eviction order.
        for marker in ['1', '2', '3', '4'].iter() {
            g.set_cell(0, 0, Cell::from(*marker));
            g.set_cell(1, 0, Cell::from(*marker));
            g.scroll_up(1, Cell::default());
        }
        // After 4 pushes into a capacity-3 ring: '1' was evicted, ring holds
        // ['2','3','4'] in chronological order.
        assert_eq!(g.scrollback_len(), 3);
        let to_string = |line: &[Cell]| line.iter().map(|c| c.ch).collect::<String>();
        assert_eq!(to_string(g.scrollback_line(0).unwrap()), "22");
        assert_eq!(to_string(g.scrollback_line(1).unwrap()), "33");
        assert_eq!(to_string(g.scrollback_line(2).unwrap()), "44");
        assert_eq!(g.scrollback_line(3), None);
    }

    #[test]
    fn clear_scrollback_zeroes_len_in_o1() {
        let mut g = Grid::with_scrollback(2, 2, 5);
        for _ in 0..5 {
            g.scroll_up(1, Cell::default());
        }
        assert_eq!(g.scrollback_len(), 5);
        g.clear_scrollback();
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_line(0), None);
    }

    #[test]
    fn scroll_up_zero_is_noop() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(0, Cell::default());
        assert_eq!(g.cell(0, 0).ch, 'A');
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn zero_capacity_scrollback_silently_drops_pushes() {
        let mut g = Grid::with_scrollback(3, 2, 0);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(1, Cell::default());
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_capacity(), 0);
    }
}
