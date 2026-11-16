//! The in-RAM scrollback ring.
//!
//! The variant a pane gets when it has no session id to hang a file
//! off, and the alternate screen's history: a bounded ring of lines,
//! so a long session's history costs a bounded amount of memory and
//! nothing else.
//!
//! A line is kept as the grid's own eight-byte cells, but numbered
//! against a palette of its own instead of the grid's table: the grid
//! gives numbers back once no cell on screen uses them, and a line in
//! history would then name somebody else's colour.  A plain line has
//! an empty palette and goes in as one copy; a coloured one looks a
//! value up once per run of cells in one colour.

use super::format::{RowHint, new_epoch};
use crate::grid::{Cell, PackedCell, PackedStyle, StyleTable};

/// How far a line's palette is searched before a value goes in again:
/// a line that changes colour every cell costs a value a cell rather
/// than a search a cell.
const PALETTE_SEARCH: usize = 32;

/// A bounded ring of lines, `cols` eight-byte cells each: at most
/// `capacity` lines, eight bytes a cell plus at most eight more for a
/// line's palette, where lines of twenty-byte cells took twenty.
pub struct MemoryScrollback {
    /// Line after line, oldest at `head`; grown as lines come, up to
    /// `capacity` lines, then reused.
    cells: Vec<PackedCell>,
    /// Per line, the values its cells' numbers index from 1 (0 is the
    /// default); a plain line's is empty and allocates nothing.  For a
    /// line kept as it was (`as_it_was`), sixteen values instead, a
    /// number's value at the number's low four bits.
    palettes: Vec<Vec<PackedStyle>>,
    /// Per line, whether its cells carry the grid's own numbers.
    as_it_was: Vec<bool>,
    pub(super) capacity: usize,
    cols: usize,
    head: usize,
    len: usize,
    /// The same idea as the file variant's, kept so that a consumer
    /// filing something against a line does not have to know which
    /// variant it is talking to.  In RAM it changes on construction
    /// only -- reflow builds a whole new `MemoryScrollback`.
    pub(super) epoch: u64,
}

impl MemoryScrollback {
    pub fn new(capacity: usize, cols: usize) -> Self {
        Self {
            cells: Vec::new(),
            palettes: Vec::new(),
            as_it_was: Vec::new(),
            capacity,
            cols,
            head: 0,
            len: 0,
            epoch: new_epoch(),
        }
    }

    /// Where the next line goes, as a line index; the oldest gives way
    /// once the ring is full.
    fn next_slot(&mut self) -> usize {
        if self.len < self.capacity {
            if self.cells.capacity() == 0 {
                // one reservation for the whole ring, filled as it grows
                self.cells.reserve_exact(self.capacity * self.cols);
                self.palettes.reserve_exact(self.capacity);
                self.as_it_was.reserve_exact(self.capacity);
            }
            self.cells.resize(self.cells.len() + self.cols, PackedCell::BLANK);
            self.palettes.push(Vec::new());
            self.as_it_was.push(false);
            self.len += 1;
            return self.len - 1;
        }
        let slot = self.head;
        self.head = (self.head + 1) % self.capacity;
        slot
    }

    pub fn push_line(&mut self, source: &[Cell]) {
        if self.capacity == 0 {
            return;
        }
        let slot = self.next_slot();
        self.as_it_was[slot] = false;
        let (row, palette) = self.slot_mut(slot);
        palette.clear();
        let n = source.len().min(row.len());
        for (dst, c) in row[..n].iter_mut().zip(source) {
            let mut attrs = c.attrs;
            attrs._pad = [0; 3];
            let local = local_number(palette, PackedStyle::pack(&attrs), n);
            *dst = PackedCell::from_cell(c, local);
        }
        row[n..].fill(PackedCell::BLANK);
    }

    /// [`Self::push_line`] for a row of the grid's own cells, numbered
    /// against `styles`.  `known` is every number the row holds, when
    /// the grid knows them: the row then goes in as one copy, with
    /// those numbers' values beside it.
    pub fn push_packed(&mut self, source: &[PackedCell], styles: &StyleTable, hint: RowHint, known: Option<&[u32]>) {
        if self.capacity == 0 {
            return;
        }
        let slot = self.next_slot();
        // sixteen values cost no more than the eight bytes a cell the
        // palette may take, and the numbers must not share low bits
        let cols = self.cols;
        let known = known.filter(|ids| !styles.is_positional() && cols >= AS_IT_WAS && apart(ids));
        self.as_it_was[slot] = known.is_some();
        let (row, palette) = self.slot_mut(slot);
        let n = source.len().min(row.len());
        if let Some(ids) = known {
            row[..n].copy_from_slice(&source[..n]);
            row[n..].fill(PackedCell::BLANK);
            // what a slot held for a line before is never read: the
            // cells carry no number but these; a plain line needs none
            if ids.is_empty() {
                palette.clear();
            } else if palette.len() != AS_IT_WAS {
                palette.clear();
                palette.resize(AS_IT_WAS, PackedStyle::DEFAULT);
            }
            for &id in ids {
                palette[id as usize % AS_IT_WAS] = styles.value_or_default(id);
            }
            return;
        }
        palette.clear();
        // past `plain_from` every cell is in the default attributes:
        // number 0 in any table, copied as it is
        let plain_from = (hint.plain_from as usize).min(n);
        let (mut theirs, mut ours) = (0u32, 0u32);
        for (dst, &c) in row[..plain_from].iter_mut().zip(source) {
            let id = c.style_id();
            if id != theirs {
                theirs = id;
                ours = if id == 0 { 0 } else { local_number(palette, styles.get(id), n) };
            }
            *dst = c.with_style(ours);
        }
        row[plain_from..n].copy_from_slice(&source[plain_from..n]);
        row[n..].fill(PackedCell::BLANK);
    }

    fn slot_mut(&mut self, slot: usize) -> (&mut [PackedCell], &mut Vec<PackedStyle>) {
        let at = slot * self.cols;
        (&mut self.cells[at..at + self.cols], &mut self.palettes[slot])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn expand(&self, slot: usize, c: PackedCell) -> Cell {
        let id = c.style_id() as usize;
        let palette = &self.palettes[slot];
        let mut attrs = if id == 0 {
            Default::default()
        } else if self.as_it_was[slot] {
            palette[id % AS_IT_WAS].unpack()
        } else {
            palette[id - 1].unpack()
        };
        if c.is_cluster() {
            attrs._pad[0] = crate::grid::FLAG_CLUSTER;
        }
        Cell { ch: c.ch(), attrs }
    }

    fn slot_of(&self, idx: usize) -> Option<usize> {
        (idx < self.len).then(|| (self.head + idx) % self.capacity)
    }

    /// Line `idx` (0 = oldest), `cols` cells wide.
    pub fn line(&self, idx: usize) -> Option<Vec<Cell>> {
        let slot = self.slot_of(idx)?;
        let row = &self.cells[slot * self.cols..(slot + 1) * self.cols];
        Some(row.iter().map(|&c| self.expand(slot, c)).collect())
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        let slot = self.slot_of(line_idx)?;
        if col >= self.cols {
            return None;
        }
        Some(self.expand(slot, self.cells[slot * self.cols + col]))
    }

    pub fn clear(&mut self) {
        // the memory stays, so a scroll storm after a clear does not
        // pay for it again
        self.head = 0;
        self.len = 0;
        self.cells.clear();
        self.palettes.clear();
        self.as_it_was.clear();
    }

    /// Whether line `idx` was kept as the grid's own cells.
    #[cfg(test)]
    pub(crate) fn kept_as_it_was(&self, idx: usize) -> bool {
        self.slot_of(idx).is_some_and(|slot| self.as_it_was[slot])
    }

    pub fn approx_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<PackedCell>()
            + self.palettes.iter().map(|p| p.capacity() * 8).sum::<usize>()
    }
}

/// The values a line kept as it was carries, one for each value of a
/// number's low four bits.
const AS_IT_WAS: usize = 16;

/// Whether no two of `ids` share their low four bits.
fn apart(ids: &[u32]) -> bool {
    let mut seen = 0u16;
    ids.iter().all(|&id| {
        let bit = 1u16 << (id as usize % AS_IT_WAS);
        let fresh = seen & bit == 0;
        seen |= bit;
        fresh
    })
}

/// `value`'s number in a line's palette, putting it in if the first
/// [`PALETTE_SEARCH`] values do not have it.  A palette never holds
/// more than `cells` values, one for each cell of its line, and grows
/// no further than that.
#[inline]
fn local_number(palette: &mut Vec<PackedStyle>, value: PackedStyle, cells: usize) -> u32 {
    if value.is_default() {
        return 0;
    }
    if let Some(i) = palette.iter().take(PALETTE_SEARCH).position(|&p| p == value) {
        return i as u32 + 1;
    }
    if palette.len() == palette.capacity() {
        let want = (palette.capacity() * 2).max(4).min(cells);
        palette.reserve_exact(want.saturating_sub(palette.len()).max(1));
    }
    palette.push(value);
    palette.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::testing::*;

    // ─── pre-existing tests follow ────────────────────────────────────

    #[test]
    fn push_grows_then_evicts() {
        let mut sb = MemoryScrollback::new(3, 4);
        sb.push_line(&fill(b'a', 4));
        sb.push_line(&fill(b'b', 4));
        sb.push_line(&fill(b'c', 4));
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'a');
        assert_eq!(sb.line(2).unwrap()[0].ch, 'c');

        // Past capacity: 'a' evicts.
        sb.push_line(&fill(b'd', 4));
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'b');
        assert_eq!(sb.line(2).unwrap()[0].ch, 'd');
    }

    /// A row of stored cells shorter than the ring's lines reads back
    /// blank past its end, even over a slot an older line used.
    #[test]
    fn a_short_stored_row_reads_blank_past_its_end() {
        let mut g = crate::grid::Grid::new(4, 1);
        let mut sb = MemoryScrollback::new(1, 4);
        sb.push_line(&fill(b'a', 4));
        g.set_cell(0, 0, Cell::from('z'));
        let row = g.live_row_at_view(0, 0).unwrap();
        sb.push_packed(&row[..2], g.styles(), RowHint::NONE, None);
        let line: String = sb.line(0).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(line, "z   ");
    }

    /// A row whose numbers the grid knows is kept as it was only when no
    /// two of them share their low four bits; one where two do is
    /// renumbered, and both read back in their own colours.
    #[test]
    fn numbers_sharing_low_bits_send_a_row_through_its_palette() {
        use crate::grid::{CellAttrs, Color, Grid};
        let mut g = Grid::new(80, 1);
        let colour = |k: u8| Cell { ch: 'x', attrs: CellAttrs { fg: Color::rgb(k, 1, 2), ..Default::default() } };
        // numbers 1 to 17, one a cell; the row's own two are 1 and 17
        for k in 0..17u8 {
            g.set_cell(k as u16, 0, colour(k));
        }
        let mut row: Vec<PackedCell> = g.live_row_at_view(0, 0).unwrap().to_vec();
        let (a, b) = (row[0], row[16]);
        assert_eq!((a.style_id(), b.style_id()), (1, 17));
        row.iter_mut().for_each(|c| *c = PackedCell::BLANK);
        row[0] = a;
        row[1] = b;
        let mut sb = MemoryScrollback::new(2, 80);
        sb.push_packed(&row, g.styles(), RowHint::NONE, Some(&[1, 17]));
        assert!(!sb.kept_as_it_was(0));
        sb.push_packed(&row[..1], g.styles(), RowHint::NONE, Some(&[1]));
        assert!(sb.kept_as_it_was(1));
        let line = sb.line(0).unwrap();
        assert_eq!((line[0], line[1]), (colour(0), colour(16)));
        assert_eq!(sb.line(1).unwrap()[0], colour(0));
        // a plain row kept as it was carries no values
        let mut plain = MemoryScrollback::new(4, 80);
        for _ in 0..4 {
            plain.push_packed(&[PackedCell::BLANK; 80], g.styles(), RowHint::NONE, Some(&[]));
        }
        assert!(plain.kept_as_it_was(3));
        assert_eq!(plain.approx_bytes(), 4 * 80 * 8);
    }

    /// A line costs eight bytes a cell, and a line that changes colour
    /// every cell eight more for its palette: never the twenty a cell
    /// lines took before.  Every line reads back as it went in, from
    /// cells and from the grid's own cells alike.
    #[test]
    fn a_line_costs_eight_bytes_a_cell_and_its_palette() {
        use crate::grid::{CellAttrs, Color, Grid};
        let (cols, lines) = (80usize, if cfg!(miri) { 20 } else { 2000 });
        let mut plain = MemoryScrollback::new(lines, cols);
        for i in 0..lines {
            plain.push_line(&fill(b'a' + (i % 26) as u8, cols));
        }
        assert_eq!(plain.len(), lines);
        assert!(plain.approx_bytes() <= lines * cols * 8, "{} bytes", plain.approx_bytes());
        assert_eq!(plain.line(lines - 1).unwrap(), fill(b'a' + ((lines - 1) % 26) as u8, cols));
        let row = |i: usize| -> Vec<Cell> {
            (0..cols)
                .map(|c| Cell {
                    ch: 'x',
                    attrs: CellAttrs { fg: Color::rgb(c as u8, i as u8, 7), ..Default::default() },
                })
                .collect()
        };
        let mut rich = MemoryScrollback::new(lines, cols);
        for i in 0..lines {
            rich.push_line(&row(i));
        }
        assert!(rich.approx_bytes() <= lines * cols * 16, "{} bytes", rich.approx_bytes());
        assert_eq!(rich.line(rich.len() - 1).unwrap(), row(lines - 1));
        assert_eq!(rich.line(0).unwrap(), row(0));
        // a ring that reuses its lines over and over holds no more
        let mut small = MemoryScrollback::new(2, cols);
        for i in 0..lines {
            small.push_line(&row(i));
        }
        assert!(small.approx_bytes() <= 2 * cols * 16, "{} bytes", small.approx_bytes());
        assert_eq!(small.line(1).unwrap(), row(lines - 1));
        let mut g = Grid::new(cols as u16, 1);
        let mut small = MemoryScrollback::new(2, cols);
        for i in 0..lines {
            for (c, cell) in row(i).iter().enumerate() {
                g.set_cell(c as u16, 0, *cell);
            }
            small.push_packed(g.live_row_at_view(0, 0).unwrap(), g.styles(), RowHint::NONE, None);
        }
        assert!(small.approx_bytes() <= 2 * cols * 16, "{} bytes", small.approx_bytes());
        assert_eq!(small.line(1).unwrap(), row(lines - 1));
        // the grid's own cells, a few colours and runs of them
        let mut g = Grid::new(cols as u16, 1);
        let mut from_grid = MemoryScrollback::new(4, cols);
        for (c, cell) in row(3).iter().enumerate() {
            let mut cell = *cell;
            cell.attrs.fg = Color::indexed((c / 7) as u8);
            g.set_cell(c as u16, 0, cell);
        }
        let want: Vec<Cell> = (0..cols as u16).map(|c| g.cell(c, 0)).collect();
        let hint = RowHint { plain_from: cols as u16, wide: false };
        from_grid.push_packed(g.live_row_at_view(0, 0).unwrap(), g.styles(), hint, None);
        assert_eq!(from_grid.line(0).unwrap(), want);
        assert!(from_grid.palettes[0].len() <= cols.div_ceil(7));
    }

    #[test]
    fn clear_drops_logical_contents() {
        let mut sb = MemoryScrollback::new(2, 3);
        sb.push_line(&fill(b'a', 3));
        sb.push_line(&fill(b'b', 3));
        sb.clear();
        assert_eq!(sb.len(), 0);
        assert!(sb.line(0).is_none());

        // Post-clear use works.
        sb.push_line(&fill(b'c', 3));
        assert_eq!(sb.len(), 1);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'c');
    }

    #[test]
    fn zero_capacity_disables_storage() {
        let mut sb = MemoryScrollback::new(0, 80);
        sb.push_line(&fill(b'x', 80));
        assert_eq!(sb.len(), 0);
        assert!(sb.line(0).is_none());
    }
}
