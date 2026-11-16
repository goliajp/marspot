//! The cell as the grid stores it: eight bytes.
//!
//! A character and the number of its attributes in the grid's
//! [`StyleTable`], instead of the attributes themselves.  Readers get a
//! [`Cell`] back from [`PackedCell::expand`]; the grid's own hot paths
//! (writing text, filing rows into history, publishing frames) work on
//! these directly, eight bytes a cell where they used to move twenty.

use super::{CLUSTER_INDEX_BASE, Cell, FLAG_CLUSTER, StyleTable};

/// Bits 0–20: the character, or a cluster pool index when bit 21 is
/// set.  Bits 22–31: zero.  Bits 32–55: the number of the cell's
/// attributes in its grid's table, 0 for the default.  Bits 56–63:
/// zero.  So a blank cell is `0x20`, and a row of them is one repeated
/// word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct PackedCell(u64);

const CH_MASK: u64 = 0x1F_FFFF;
const CLUSTER_BIT: u64 = 1 << 21;
const STYLE_SHIFT: u32 = 32;
/// Bits no cell has set: 22–31 and 56–63.
const RESERVED: u64 = 0x3FF << 22 | 0xFF << 56;

impl Default for PackedCell {
    fn default() -> Self {
        PackedCell::BLANK
    }
}

impl PackedCell {
    /// A space in the default attributes.
    pub const BLANK: PackedCell = PackedCell(0x20);

    /// `ch` under attribute number `id`.
    #[inline(always)]
    pub const fn new(ch: char, id: u32) -> PackedCell {
        PackedCell(ch as u64 | (id as u64) << STYLE_SHIFT)
    }

    /// A one-byte character under attribute number `id`.  Every byte
    /// is a character (U+0000–U+00FF).
    #[inline(always)]
    pub const fn byte(b: u8, id: u32) -> PackedCell {
        PackedCell(b as u64 | (id as u64) << STYLE_SHIFT)
    }

    /// `cell`'s character and cluster flag, under number `id`.
    #[inline(always)]
    pub fn from_cell(cell: &Cell, id: u32) -> PackedCell {
        let flag = if cell.attrs._pad[0] & FLAG_CLUSTER != 0 { CLUSTER_BIT } else { 0 };
        PackedCell(cell.ch as u64 | flag | (id as u64) << STYLE_SHIFT)
    }

    /// The cell as readers see it.
    #[inline(always)]
    pub fn expand(self, styles: &StyleTable) -> Cell {
        let mut attrs = styles.get(self.style_id()).unpack();
        if self.is_cluster() {
            attrs._pad[0] = FLAG_CLUSTER;
        }
        Cell { ch: self.ch(), attrs }
    }

    #[inline(always)]
    pub fn ch(self) -> char {
        // SAFETY: every constructor takes a `char` or a byte, and the
        // setters below do too, so the low 21 bits always hold one.
        unsafe { char::from_u32_unchecked(self.ch_bits()) }
    }

    /// The character's scalar value (or pool index), without the flag.
    #[inline(always)]
    pub const fn ch_bits(self) -> u32 {
        (self.0 & CH_MASK) as u32
    }

    #[inline(always)]
    pub const fn style_id(self) -> u32 {
        (self.0 >> STYLE_SHIFT) as u32
    }

    #[inline(always)]
    pub const fn is_cluster(self) -> bool {
        self.0 & CLUSTER_BIT != 0
    }

    /// The pool index this cell points at, if it points at one.
    #[inline(always)]
    pub fn cluster_index(self) -> Option<u32> {
        self.is_cluster().then(|| self.ch_bits().checked_sub(CLUSTER_INDEX_BASE)).flatten()
    }

    /// What decides where a run of cells filed into history ends: the
    /// attribute number and the cluster flag.  Zero for a plain cell.
    #[inline(always)]
    pub const fn run_key(self) -> u64 {
        self.0 >> 21
    }

    /// The same cell holding `ch`, no longer a cluster.
    #[inline(always)]
    pub fn with_char(self, ch: char) -> PackedCell {
        PackedCell(self.0 & !(CH_MASK | CLUSTER_BIT) | ch as u64)
    }

    /// The same cell pointing at pool entry `ch`.
    #[inline(always)]
    pub fn with_cluster_char(self, ch: char) -> PackedCell {
        PackedCell(self.0 & !CH_MASK | ch as u64 | CLUSTER_BIT)
    }

    /// The same cell under attribute number `id`.
    #[inline(always)]
    pub const fn with_style(self, id: u32) -> PackedCell {
        PackedCell(self.0 & ((1 << STYLE_SHIFT) - 1) | (id as u64) << STYLE_SHIFT)
    }

    /// The eight bytes, as they go into a frame.
    #[inline(always)]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The cell, if it is one a grid could hold with a table of `count`
    /// values, else a blank.  For cells that came from another
    /// process: the bits are not trusted until this has looked at them.
    #[inline(always)]
    pub fn checked(self, count: u32) -> PackedCell {
        let ok = self.0 & RESERVED == 0
            && self.style_id() < count
            && char::from_u32(self.ch_bits()).is_some();
        if ok { self } else { PackedCell::BLANK }
    }
}
