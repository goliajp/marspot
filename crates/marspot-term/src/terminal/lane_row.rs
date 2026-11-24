//! The text lane's common case on its own: a run of ASCII that ends
//! inside the row, with a control or the end of the input behind it.

use super::super::Handler;
use super::{write_cells, write_chars};
use crate::grid::StoredCell;

impl<'a> Handler<'a> {
    /// Write the `n` printable ASCII bytes at the head of `bytes` from
    /// column `col` of `row` on, and leave the last of them as the open
    /// cluster.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)] // the lane's own locals, handed over as they are
    pub(super) fn ascii_in_row(
        &mut self,
        bytes: &[u8],
        n: usize,
        (col, row): (usize, u16),
        cols: usize,
        plain: bool,
        notes: bool,
        pen: u32,
    ) {
        let to = col + n;
        let (cells, mark) = self.grid.row_and_mark_mut(row);
        if plain && col >= mark.end as usize {
            if n < 8 {
                // too short to be worth a call
                for (cell, &b) in cells[col..to].iter_mut().zip(bytes) {
                    *cell = StoredCell::byte(b, 0);
                }
            } else {
                write_chars(&mut cells[col..to], bytes);
            }
        } else {
            write_cells(&mut cells[col..to], bytes, pen);
            if !plain && to > mark.plain_from as usize {
                mark.plain_from = to as u16;
            }
            if notes {
                mark.styles.note(pen);
            }
        }
        if to > mark.end as usize {
            mark.end = to as u16;
        }
        if to < cols {
            self.grid.set_cursor(to as u16, row);
        } else {
            self.grid.set_cursor((cols - 1) as u16, row);
            *self.pending_wrap = true;
        }
        let last = bytes[n - 1];
        let ch = last as char;
        self.cluster_buf.clear();
        // the run is printable ASCII: its last byte is a whole
        // character, pushed without `String::push`'s encoder,
        // which LLVM may leave out of line here
        // SAFETY: a byte below 0x80 is valid UTF-8 on its own
        unsafe { self.cluster_buf.as_mut_vec().push(last) };
        *self.cluster_fast = Some((ch, 1));
        *self.cluster_anchor = Some(((to - 1) as u16, row, 1));
        *self.seg_synced = false;
    }
}
