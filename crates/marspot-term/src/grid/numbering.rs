//! Keeping the grid's cells and its table of attribute sets in step:
//! every write of a cell goes through here or through the lane, which
//! takes its number from [`Grid::pen_id`].

use super::{Cell, CellAttrs, Grid, PackedStyle, StoredCell, StyleTable};

/// Cells looked at per step of a sweep.  A step is a few microseconds,
/// far under the latency a keystroke's echo can notice.
const SWEEP_CELLS_PER_STEP: usize = 4096;

impl Grid {
    /// The table the cells' numbers refer to.
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// The number cell `idx` should carry for `attrs`.
    #[inline(always)]
    fn id_for(&mut self, idx: usize, attrs: &CellAttrs) -> u32 {
        let p = PackedStyle::pack(attrs);
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
        if self.styles.wants_sweep() || (self.styles.is_marking() && self.styles.fresh_since_step()) {
            self.sweep_step(id);
        }
        Some(id)
    }

    /// The number the text lane writes for the pen's attributes, or
    /// `None` when cells are numbered by position and the lane has to
    /// leave the run to the per-character path.
    #[inline(always)]
    pub fn pen_id(&mut self, attrs: &CellAttrs) -> Option<u32> {
        let p = PackedStyle::pack(attrs);
        if p.is_default() {
            return Some(0);
        }
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
    #[inline(always)]
    pub(super) fn put(&mut self, idx: usize, cell: Cell) {
        let mut attrs = cell.attrs;
        attrs._pad = [0; 3];
        let id = self.id_for(idx, &attrs);
        self.cells[idx] = cell;
        self.ids[idx] = id;
    }

    /// Write `cell` over `range`.
    pub(super) fn fill(&mut self, range: std::ops::Range<usize>, cell: Cell) {
        for idx in range {
            self.put(idx, cell);
        }
    }

    /// Copy `cols` cells from `src` to `dst`, both storage indices.
    pub(super) fn copy_cells(&mut self, src: usize, dst: usize, cols: usize) {
        self.cells.copy_within(src..src + cols, dst);
        if self.styles.is_positional() {
            for i in 0..cols {
                let id = self.ids[src + i];
                let s = if id == 0 { PackedStyle::DEFAULT } else { self.styles.get(id) };
                self.ids[dst + i] = self.styles.set_at(dst + i, s);
            }
            return;
        }
        self.ids.copy_within(src..src + cols, dst);
        if self.styles.is_marking() {
            // a row already looked at may be receiving the only copy of
            // a number from one not yet looked at
            self.styles.mark_all(self.ids[dst..dst + cols].iter().copied());
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
        self.cells = vec![StoredCell::default(); n];
        self.ids = vec![0; n];
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
        self.ids.iter_mut().for_each(|id| *id = 0);
        self.cells.iter_mut().for_each(|c| *c = StoredCell::default());
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
        }
        self.styles.mark(held);
        let cols = self.cols as usize;
        let rows = (SWEEP_CELLS_PER_STEP / cols).max(1);
        let from = self.sweep_row as usize;
        let to = (from + rows).min(self.rows as usize);
        self.styles.mark_all(self.ids[from * cols..to * cols].iter().copied());
        self.sweep_row = to as u16;
        if to == self.rows as usize {
            self.styles.end_sweep();
        }
    }

    /// Run a sweep to its end now, starting one if none is running.
    fn sweep_all(&mut self) {
        if !self.styles.is_marking() {
            self.styles.begin_sweep();
            self.sweep_row = 0;
        }
        let cols = self.cols as usize;
        let from = self.sweep_row as usize;
        self.styles.mark_all(self.ids[from * cols..].iter().copied());
        self.sweep_row = self.rows;
        self.styles.end_sweep();
    }

    /// Number every cell by its position: the shared way is out of
    /// numbers even after a sweep.
    #[cold]
    fn go_positional(&mut self) {
        let values: Vec<PackedStyle> = self.ids.iter().map(|&id| self.styles.get(id)).collect();
        self.styles.make_positional(self.cells.len());
        for (idx, s) in values.into_iter().enumerate() {
            self.ids[idx] = self.styles.set_at(idx, s);
        }
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
            let s = self.styles.get(self.ids[idx]);
            match shared.intern(s) {
                Some(new) if shared.used() as usize <= n / 4 + 1 => *id = new,
                _ => return,
            }
        }
        self.styles = shared;
        self.ids = ids;
        self.sweep_row = 0;
    }

    /// Whether every cell's number names its own attributes, and
    /// positional numbers sit where they belong.  For tests, fuzzing
    /// and debug builds.
    pub fn check_styles(&self) -> Result<(), String> {
        for (idx, (c, &id)) in self.cells.iter().zip(&self.ids).enumerate() {
            let mut a = c.attrs;
            a._pad = [0; 3];
            if (id as usize) >= self.styles.len() {
                return Err(format!("cell {idx}: number {id} past the table"));
            }
            let held = self.styles.get(id);
            if held != PackedStyle::pack(&a) {
                return Err(format!("cell {idx}: number {id} holds {held:?}, the cell has {a:?}"));
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
