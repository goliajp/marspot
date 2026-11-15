//! The in-RAM scrollback ring.
//!
//! The variant a pane gets when it has no session id to hang a file
//! off, and the alternate screen's history: a bounded ring of lines,
//! so a long session's history costs a bounded amount of memory and
//! nothing else.
//!
//! A line is kept as the record the file variant writes for it --
//! runs of cells that share their attributes, a byte a cell for plain
//! text -- not as twenty bytes a cell.  It cannot be kept as the
//! grid's own eight-byte cells: their attribute numbers belong to the
//! grid's table, which gives numbers back once no cell on screen uses
//! them.

use super::file::ring::Ring;
use super::format::{REC_PACKED, RowHint, encode_packed_with, new_epoch};
use super::format_packed::{append_cells, encode_packed_cells};
use crate::grid::{Cell, PackedCell, StyleTable};

/// A bounded ring of encoded lines.
///
/// At most `capacity` lines, and so at most what `capacity` lines of
/// twenty-byte cells took when lines were kept that way: a record is
/// never longer than its cells as twenty bytes each plus three, and a
/// line's buffer is only ever reused for a record no longer than one
/// the ring held before.  A screen of plain text costs about a byte a
/// cell.
pub struct MemoryScrollback {
    ring: Ring,
    pub(super) capacity: usize,
    cols: usize,
    /// The record being built, reused for every line.
    scratch: Vec<u8>,
    /// The same idea as the file variant's, kept so that a consumer
    /// filing something against a line does not have to know which
    /// variant it is talking to.  In RAM it changes on construction
    /// only -- reflow builds a whole new `MemoryScrollback`.
    pub(super) epoch: u64,
}

impl MemoryScrollback {
    pub fn new(capacity: usize, cols: usize) -> Self {
        Self {
            // bounded by its lines; see the type's comment
            ring: Ring::with_bytes(capacity, usize::MAX),
            capacity,
            cols,
            scratch: Vec::new(),
            epoch: new_epoch(),
        }
    }

    pub fn push_line(&mut self, source: &[Cell]) {
        if self.capacity == 0 {
            return;
        }
        // wider than the ring's lines: the rest is cut, as it always was
        let source = &source[..source.len().min(self.cols)];
        let n = source.iter().rposition(|c| *c != Cell::default()).map_or(0, |i| i + 1);
        let line = &source[..n];
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0; 3]);
        let packed = n > 0 && encode_packed_with(line, RowHint::NONE, &mut self.scratch);
        if !packed {
            self.scratch.extend_from_slice(Cell::slice_as_bytes(line));
        }
        self.keep(n, packed);
    }

    /// [`Self::push_line`] for a row of the grid's own cells, encoded
    /// straight from them.
    pub fn push_packed(&mut self, source: &[PackedCell], styles: &StyleTable, hint: RowHint) {
        if self.capacity == 0 {
            return;
        }
        let source = &source[..source.len().min(self.cols)];
        let n = source.iter().rposition(|c| *c != PackedCell::BLANK).map_or(0, |i| i + 1);
        let line = &source[..n];
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0; 3]);
        let packed = n > 0 && encode_packed_cells(line, styles, hint, &mut self.scratch);
        if !packed {
            append_cells(line, styles, &mut self.scratch);
        }
        self.keep(n, packed);
    }

    /// The record in `scratch`, `cols` cells long, into the ring.
    fn keep(&mut self, cols: usize, packed: bool) {
        self.scratch[0] = if packed { REC_PACKED } else { 0 };
        self.scratch[1..3].copy_from_slice(&(cols as u16).to_le_bytes());
        self.ring.push(&self.scratch);
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Line `idx` (0 = oldest), `cols` cells wide.
    pub fn line(&self, idx: usize) -> Option<Vec<Cell>> {
        if idx >= self.ring.len() {
            return None;
        }
        Some(self.ring.with_row(idx, |row| {
            let mut v = row[..row.len().min(self.cols)].to_vec();
            v.resize(self.cols, Cell::default());
            v
        }))
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        if line_idx >= self.ring.len() || col >= self.cols {
            return None;
        }
        Some(self.ring.with_row(line_idx, |row| row.get(col).copied().unwrap_or_default()))
    }

    pub fn clear(&mut self) {
        self.ring.clear();
    }

    pub fn approx_bytes(&self) -> usize {
        self.ring.bytes() + self.scratch.capacity()
    }
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
        sb.push_packed(&row[..2], g.styles(), RowHint::NONE);
        let line: String = sb.line(0).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(line, "z   ");
    }

    /// Plain text costs about a byte a cell; a line that changes
    /// colour every cell costs no more than twenty bytes a cell did,
    /// and the lines read back as they went in.
    #[test]
    fn a_line_costs_what_its_record_does() {
        use crate::grid::{CellAttrs, Color};
        let (cols, lines) = (80usize, if cfg!(miri) { 20 } else { 2000 });
        let mut plain = MemoryScrollback::new(lines, cols);
        for i in 0..lines {
            plain.push_line(&fill(b'a' + (i % 26) as u8, cols));
        }
        assert_eq!(plain.len(), lines);
        assert!(plain.approx_bytes() < lines * (cols + 64), "{} bytes", plain.approx_bytes());
        assert_eq!(plain.line(lines - 1).unwrap(), fill(b'a' + ((lines - 1) % 26) as u8, cols));
        // a short line costs its text, not its width
        let mut short = MemoryScrollback::new(lines, cols);
        let mut line = fill(b'x', 2);
        line.resize(cols, Cell::default());
        for _ in 0..lines {
            short.push_line(&line);
        }
        assert!(short.approx_bytes() < lines * 48, "{} bytes", short.approx_bytes());
        assert_eq!(short.line(0).unwrap(), line);
        let mut rich = MemoryScrollback::new(lines, cols);
        let row = |i: usize| -> Vec<Cell> {
            (0..cols)
                .map(|c| Cell {
                    ch: 'x',
                    attrs: CellAttrs { fg: Color::rgb(c as u8, i as u8, 7), ..Default::default() },
                })
                .collect()
        };
        for i in 0..lines {
            rich.push_line(&row(i));
        }
        use crate::grid::CELL_MEM_BYTES;
        let bound = lines * (cols * CELL_MEM_BYTES + 3) + 4 * cols * CELL_MEM_BYTES + 256;
        assert!(rich.approx_bytes() <= bound, "{} bytes, bound {bound}", rich.approx_bytes());
        let newest = rich.len() - 1;
        assert_eq!(rich.line(newest).unwrap(), row(lines - 1));
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
