//! The cells of a frame, in either of the two layouts a frame can have.
//!
//! The header's `cell_size` says which, frame by frame:
//!
//! - 20: `cols × rows` cells of twenty bytes each, a `Cell` as it is
//!   in memory.  Every reader reads it; it is what a session writes
//!   until its reader has said it reads the other one.
//! - 8: `cols × rows` eight-byte cells, a `PackedCell` each (the
//!   character, the cluster flag and a number), then the table the
//!   numbers index: a `u32` count, four zero bytes, and that many
//!   eight-byte `PackedStyle` values, so every eight-byte value sits
//!   on an eight-byte boundary.  Only written with the header's `version`
//!   at [`VERSION_PACKED`], which no reader from before this layout
//!   accepts, and only to a reader that said it reads it.
//!
//! The region keeps room for twenty bytes a cell whatever the frame
//! uses, so the wrap flags and the identity block stay where every
//! build expects them.
//!
//! A frame comes from another process: before anything reads a cell
//! of an eight-byte frame, [`FrameBuf`] checks every cell and value
//! and blanks what is not one this build could have written.

use crate::grid::{Cell, PackedCell, PackedStyle};

use super::{HEADER_BYTES, MAX_CELLS};

/// Room the region keeps per cell, whatever the frame's layout.
pub(super) const REGION_CELL_BYTES: usize = 20;
/// A frame of twenty-byte cells.
pub const CELL_SIZE_WIDE: u32 = 20;
/// A frame of eight-byte cells and their table.
pub const CELL_SIZE_PACKED: u32 = 8;
/// The header's version while a frame of eight-byte cells is in the
/// region: readers from before that layout refuse the region instead
/// of reading eight-byte cells as twenty-byte ones.
pub const VERSION_PACKED: u32 = 4;

/// The most values the table of a frame of `n` cells can have: what
/// fits between the cells and the end of the cell area.
pub(super) fn max_styles(n: usize) -> usize {
    (REGION_CELL_BYTES * MAX_CELLS).saturating_sub(n * 8 + 8) / 8
}

/// A frame's cells, as read: one of the two buffers holds them.
#[derive(Default)]
pub struct FrameBuf {
    /// The cells of a twenty-byte frame.
    pub wide: Vec<Cell>,
    /// The cells of an eight-byte frame, and the values their numbers
    /// index; checked, so every cell is one a grid could hold and
    /// every number is in the table.
    pub packed: Vec<PackedCell>,
    pub styles: Vec<PackedStyle>,
    /// Which of the two the last frame used.
    pub is_packed: bool,
}

impl FrameBuf {
    /// Copy the cells of a frame of `n` cells laid out as `cell_size`
    /// says, from `base`.  False for a layout or a table size no build
    /// writes, which only a torn read produces: the caller reads again.
    ///
    /// # Safety
    /// `base` is a mapped region of at least the full capacity, and the
    /// caller discards what this copied unless the frame's sequence
    /// number held across the copy.
    pub(super) unsafe fn copy_from(&mut self, base: *const u8, n: usize, cell_size: u32) -> bool {
        let cells = unsafe { base.add(HEADER_BYTES) };
        match cell_size {
            CELL_SIZE_WIDE => {
                self.wide.clear();
                self.wide.reserve(n);
                // SAFETY: the region holds room for `MAX_CELLS` such
                // cells and `n` is at most that; a torn copy is thrown
                // away by the caller before a cell is read
                unsafe {
                    std::ptr::copy_nonoverlapping(cells as *const Cell, self.wide.as_mut_ptr(), n);
                    self.wide.set_len(n);
                }
                self.is_packed = false;
                true
            }
            CELL_SIZE_PACKED => {
                let count = unsafe { (cells.add(n * 8) as *const u32).read_unaligned() } as usize;
                if count == 0 || count > max_styles(n) {
                    return false;
                }
                self.packed.clear();
                self.packed.reserve(n);
                self.styles.clear();
                self.styles.reserve(count);
                // SAFETY: both stay inside the cell area (`max_styles`);
                // the two types are a `u64` each, any bits of which are
                // a value until `check` has looked at them
                unsafe {
                    std::ptr::copy_nonoverlapping(cells as *const PackedCell, self.packed.as_mut_ptr(), n);
                    self.packed.set_len(n);
                    std::ptr::copy_nonoverlapping(
                        cells.add(n * 8 + 8) as *const PackedStyle,
                        self.styles.as_mut_ptr(),
                        count,
                    );
                    self.styles.set_len(count);
                }
                self.is_packed = true;
                true
            }
            _ => false,
        }
    }

    /// After a whole frame was copied: blank every cell and value that
    /// is not one a grid could hold, and give back the buffer the other
    /// layout used, so a pane holds one frame's worth, not two.
    pub(super) fn settle(&mut self) {
        if self.is_packed {
            self.styles[0] = PackedStyle::DEFAULT;
            for s in &mut self.styles {
                if !s.is_valid() {
                    *s = PackedStyle::DEFAULT;
                }
            }
            let count = self.styles.len() as u32;
            for c in &mut self.packed {
                *c = c.checked(count);
            }
            if self.wide.capacity() > 0 {
                self.wide = Vec::new();
            }
        } else if self.packed.capacity() > 0 {
            self.packed = Vec::new();
            self.styles = Vec::new();
        }
    }

    /// The frame's cells as `Cell`s, whichever layout it came in.
    pub fn expanded(&self) -> Vec<Cell> {
        if !self.is_packed {
            return self.wide.clone();
        }
        self.packed
            .iter()
            .map(|c| {
                let mut attrs = self.styles[c.style_id() as usize].unpack();
                if c.is_cluster() {
                    attrs._pad[0] = crate::grid::FLAG_CLUSTER;
                }
                Cell { ch: c.ch(), attrs }
            })
            .collect()
    }
}
