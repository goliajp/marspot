//! Keeping the grid's cells and its table of attribute sets in step:
//! every write of a cell goes through here or through the lane, which
//! takes its number from [`Grid::pen_id`].

use super::{Cell, CellAttrs, Grid, PackedCell, PackedStyle, StyleTable};

/// Cells looked at per step of a sweep.  A step is a few microseconds,
/// far under the latency a keystroke's echo can notice.
const SWEEP_CELLS_PER_STEP: usize = 4096;

impl Grid {
    /// The table the cells' numbers refer to.
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// The number cell `idx` should carry for `attrs`.  Kept out of
    /// line: inlined, it made every writer of a cell too big to inline
    /// in turn.
    #[inline(never)]
    fn id_for(&mut self, idx: usize, attrs: &CellAttrs) -> u32 {
        self.id_for_packed(idx, PackedStyle::pack(attrs))
    }

    #[inline]
    fn id_for_packed(&mut self, idx: usize, p: PackedStyle) -> u32 {
        if self.styles.is_positional() {
            return self.styles.set_at(idx, p);
        }
        self.shared_id(p).unwrap_or_else(|| {
            self.go_positional();
            self.styles.set_at(idx, p)
        })
    }

    /// The number for `p` the shared way, sweeping when it is due, or
    /// `None` when even a full sweep leaves no room for it.
    #[inline(always)]
    fn shared_id(&mut self, p: PackedStyle) -> Option<u32> {
        let id = match self.styles.intern(p) {
            Some(id) => id,
            None => {
                self.sweep_all();
                self.styles.intern(p)?
            }
        };
        if self.styles.step_due() {
            self.sweep_step(id);
        }
        Some(id)
    }

    /// The number the text lane writes for the pen's attributes, or
    /// `None` when cells are numbered by position and the lane has to
    /// leave the run to the per-character path.
    ///
    /// Good only until the grid's next write of anything else: a fill
    /// can sweep the table or renumber the grid.
    ///
    /// The pen is not the default: [`CellAttrs::is_plain`] is false, so
    /// it never packs to number 0's value.
    #[inline(always)]
    pub fn pen_id(&mut self, attrs: &CellAttrs) -> Option<u32> {
        debug_assert!(!attrs.is_plain());
        let p = PackedStyle::pack(attrs);
        if self.styles.is_positional() {
            return None;
        }
        match self.shared_id(p) {
            Some(id) => Some(id),
            None => {
                self.go_positional();
                None
            }
        }
    }

    /// Write `cell` at storage index `idx`.
    #[inline]
    pub(super) fn put(&mut self, idx: usize, cell: Cell) {
        // the default attributes are number 0 in either numbering
        if cell.attrs.is_plain() {
            self.cells[idx] = PackedCell::from_cell(&cell, 0);
            return;
        }
        let mut attrs = cell.attrs;
        attrs._pad = [0; 3];
        let id = self.id_for(idx, &attrs);
        self.cells[idx] = PackedCell::from_cell(&cell, id);
    }

    /// Put a frame another grid published into this one: `cells` in
    /// logical order, their numbers indexing `styles`, which are that
    /// grid's.  Every number is looked up in this grid's own table --
    /// once per number per frame, kept in `remap` -- so the two tables
    /// never have to agree.  For a reader's mirror of a session's
    /// screen; the cells are as checked as `FrameBuf` leaves them.
    pub fn load_frame(&mut self, cells: &[PackedCell], styles: &[PackedStyle], remap: &mut Vec<u32>) {
        debug_assert_eq!(cells.len(), self.cells.len());
        self.top_row = 0;
        remap.clear();
        remap.resize(styles.len(), u32::MAX);
        for (idx, &c) in cells.iter().enumerate().take(self.cells.len()) {
            let theirs = c.style_id() as usize;
            let ours = if theirs == 0 {
                0
            } else if self.styles.is_positional() {
                self.styles.set_at(idx, styles[theirs])
            } else if remap[theirs] != u32::MAX {
                let id = remap[theirs];
                // a number written while a sweep runs is marked as it
                // is written, wherever the writer got it
                if self.styles.is_marking() {
                    self.styles.mark(id);
                }
                id
            } else {
                let id = self.id_for_packed(idx, styles[theirs]);
                remap[theirs] = id;
                id
            };
            self.cells[idx] = c.with_style(ours);
        }
        self.forget_row_marks();
    }

    /// Write `cell` over `range`.
    pub(super) fn fill(&mut self, range: std::ops::Range<usize>, cell: Cell) {
        if cell == Cell::default() {
            self.cells[range].fill(PackedCell::BLANK);
            return;
        }
        for idx in range {
            self.put(idx, cell);
        }
    }

    /// Copy `cols` cells from `src` to `dst`, both storage indices.
    pub(super) fn copy_cells(&mut self, src: usize, dst: usize, cols: usize) {
        self.cells.copy_within(src..src + cols, dst);
        if self.styles.is_positional() {
            for i in 0..cols {
                let c = self.cells[dst + i];
                let id = c.style_id();
                if id != 0 {
                    let s = self.styles.get(id);
                    self.cells[dst + i] = c.with_style(self.styles.set_at(dst + i, s));
                }
            }
            return;
        }
        if self.styles.is_marking() {
            // a row already looked at may be receiving the only copy of
            // a number from one not yet looked at
            let (cells, styles) = (&self.cells, &mut self.styles);
            styles.mark_all(cells[dst..dst + cols].iter().map(|c| c.style_id()));
        }
    }

    /// Replace every cell with `cells` (row-major, logical order) at a
    /// new size.  Numbering starts over the shared way.
    pub(super) fn replace_cells(&mut self, cols: u16, rows: u16, cells: &[Cell]) {
        let n = cols as usize * rows as usize;
        debug_assert_eq!(cells.len(), n);
        self.cols = cols;
        self.rows = rows;
        self.top_row = 0;
        self.cells = vec![PackedCell::BLANK; n];
        self.styles.resize(n);
        self.sweep_row = 0;
        for (idx, &c) in cells.iter().enumerate() {
            if c != Cell::default() {
                self.put(idx, c);
            }
        }
    }

    /// Every cell becomes `fill`; the table starts over.
    pub fn clear_all(&mut self, fill: Cell) {
        self.styles.reset();
        self.sweep_row = 0;
        self.cells.fill(PackedCell::BLANK);
        if fill != Cell::default() {
            self.fill(0..self.cells.len(), fill);
        }
        self.reset_row_marks_for_fill(fill);
    }

    /// One step of a sweep: start one if it is due, look at the next
    /// few rows, and end it once every row has been looked at.  `held`
    /// is a number the caller is about to write and must survive.
    fn sweep_step(&mut self, held: u32) {
        self.styles.stepped();
        if self.styles.wants_sweep() {
            self.styles.begin_sweep();
            self.sweep_row = 0;
        } else if !self.styles.is_marking() {
            // the sweep this step was owed to was given up or reset
            return;
        }
        self.styles.mark(held);
        let cols = self.cols as usize;
        let rows = (SWEEP_CELLS_PER_STEP / cols).max(1);
        let from = self.sweep_row as usize;
        let to = (from + rows).min(self.rows as usize);
        self.mark_cells(from * cols..to * cols);
        self.sweep_row = to as u16;
        if to == self.rows as usize {
            self.end_sweep();
        }
    }

    /// Run a sweep to its end now, starting one if none is running.
    fn sweep_all(&mut self) {
        if !self.styles.is_marking() {
            self.styles.begin_sweep();
            self.sweep_row = 0;
        }
        let cols = self.cols as usize;
        self.mark_cells(self.sweep_row as usize * cols..self.cells.len());
        self.sweep_row = self.rows;
        self.end_sweep();
    }

    fn mark_cells(&mut self, range: std::ops::Range<usize>) {
        let (cells, styles) = (&self.cells, &mut self.styles);
        styles.mark_all(cells[range].iter().map(|c| c.style_id()));
    }

    fn end_sweep(&mut self) {
        // every number a cell holds was marked: the sweep's whole claim
        #[cfg(debug_assertions)]
        for (idx, c) in self.cells.iter().enumerate() {
            assert!(self.styles.is_marked(c.style_id()), "cell {idx} holds unmarked {}", c.style_id());
        }
        self.styles.end_sweep();
    }

    /// Number every cell by its position: the shared way is out of
    /// numbers even after a sweep.
    #[cold]
    fn go_positional(&mut self) {
        // one pass: each cell's value goes to its own slot as the cell
        // takes the slot's number
        let mut values = vec![PackedStyle::DEFAULT; self.cells.len() + 1];
        for (idx, c) in self.cells.iter_mut().enumerate() {
            let id = c.style_id();
            if id != 0 {
                values[idx + 1] = self.styles.get(id);
                *c = c.with_style(idx as u32 + 1);
            }
        }
        self.styles.make_positional(values);
        self.forget_row_styles();
    }

    /// Go back to the shared way if the cells now use no more than a
    /// quarter as many distinct attribute sets as there are cells.  For
    /// the moments a screen starts over -- a resize, a switch of screen
    /// -- not for ordinary output, so a screen near the bound does not
    /// switch back and forth.
    pub fn share_if_few(&mut self) {
        if !self.styles.is_positional() {
            return;
        }
        let n = self.cells.len();
        let mut shared = StyleTable::new(n);
        let mut ids = vec![0u32; n];
        for (idx, id) in ids.iter_mut().enumerate() {
            let s = self.styles.get(self.cells[idx].style_id());
            match shared.intern(s) {
                Some(new) if shared.used() as usize <= n / 4 + 1 => *id = new,
                _ => return,
            }
        }
        for (c, id) in self.cells.iter_mut().zip(ids) {
            *c = c.with_style(id);
        }
        shared.succeed(&self.styles);
        self.styles = shared;
        self.forget_row_styles();
        self.sweep_row = 0;
    }

    /// Whether every cell's number is one the table holds, and
    /// positional numbers sit where they belong.  For tests, fuzzing
    /// and debug builds.
    pub fn check_styles(&self) -> Result<(), String> {
        for (idx, c) in self.cells.iter().enumerate() {
            let id = c.style_id();
            if !self.styles.holds(id) {
                return Err(format!("cell {idx}: number {id} is not in the table"));
            }
            if self.styles.is_positional() && id != 0 && id as usize != idx + 1 {
                return Err(format!("cell {idx}: positional number {id}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "numbering_tests.rs"]
mod tests;
