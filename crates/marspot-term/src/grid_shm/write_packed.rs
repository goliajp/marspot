//! Writing a frame of eight-byte cells: the live screen as the grid
//! holds it, a row at a time, then the grid's table of attributes.
//! The layout is described in `cells.rs`.

use super::{GridShmWriter, HEADER_BYTES, MAX_CELLS};
use crate::grid::{Grid, PackedCell, PackedStyle};

// the cell area starts right after the header, eight-byte aligned
const _: () = assert!(HEADER_BYTES.is_multiple_of(8));

impl GridShmWriter {
    /// The cells, the table, the cluster text and the wrap flags of the
    /// live screen.  Called between the odd and the even sequence
    /// number, with the header written.
    ///
    /// # Safety
    /// The caller holds the write side of the seqlock and has checked
    /// the grid's dims against the region's capacity.
    pub(super) unsafe fn write_packed(&mut self, grid: &Grid, seq: u64) {
        let (cols, rows) = (grid.cols() as usize, grid.rows() as usize);
        let n = cols * rows;
        debug_assert!(n <= MAX_CELLS);
        let wrapped = unsafe { self.wrapped_ptr() };
        let links = grid.link_pool_len() > 0;
        let mut table = self.clusters.as_mut().filter(|t| grid.cluster_pool_len() > 0 || links || t.last_count > 0);
        if let Some(t) = table.as_mut() {
            t.begin();
        }
        unsafe {
            let dst = self.base.add(HEADER_BYTES) as *mut PackedCell;
            for row in 0..rows {
                let src = grid.live_row_at_view(0, row as u16).expect("a live row at offset 0");
                // SAFETY: `n` cells fit the cell area, and both sides
                // are eight-byte aligned: the area starts 56 bytes into
                // a page-aligned mapping
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst.add(row * cols), cols);
                if links {
                    // ids are numbers in this grid's pool, and a reader
                    // from before links refuses a cell carrying one; the
                    // links themselves go into the table
                    for (col, c) in src.iter().enumerate().filter(|(_, c)| c.link_id() != 0) {
                        std::ptr::write(dst.add(row * cols + col), c.with_link(0));
                    }
                    if let Some(t) = table.as_mut()
                        && grid.row_links_at_view(0, row as u16, &mut self.row_links)
                    {
                        for (c0, c1, uri) in &self.row_links {
                            t.push_link(row as u16, *c0, *c1, uri);
                        }
                    }
                }
                if let Some(t) = table.as_mut() {
                    for (col, c) in src.iter().enumerate().filter(|(_, c)| c.is_cluster()) {
                        if let Some(text) = c.cluster_index().and_then(|i| grid.cluster_text_at(i)) {
                            t.push(col as u16, row as u16, text);
                        }
                    }
                }
            }
            // the table: a count, four zero bytes, then the values
            let styles = grid.styles().values();
            debug_assert!(styles.len() <= super::cells::max_styles(n));
            let at = self.base.add(HEADER_BYTES + n * 8);
            (at as *mut u32).write(styles.len() as u32);
            (at.add(4) as *mut u32).write(0);
            std::ptr::copy_nonoverlapping(styles.as_ptr(), at.add(8) as *mut PackedStyle, styles.len());
            for row in 0..rows {
                std::ptr::write(wrapped.add(row), grid.row_wrapped(row as u16) as u8);
            }
        }
        if let Some(t) = table {
            t.finish(seq);
        }
    }
}
