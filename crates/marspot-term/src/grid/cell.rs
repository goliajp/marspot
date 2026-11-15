//! What one grid cell is: a character and its attributes.

/// One grid cell.
///
/// `repr(C)` with no padding anywhere, down to the colour bytes: a row
/// of cells is written to `scrollback.bin` as its own memory, one copy
/// per line instead of an encode per cell.  That copy is only sound if
/// every byte of a `Cell` is initialised, which is what the explicit
/// `CellAttrs::_pad` and `Color`'s zeroed unused bytes are for.  The
/// layout is asserted below so a field change fails the build rather
/// than silently changing what goes to disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Cell {
    pub ch: char,
    pub attrs: CellAttrs,
}

/// Bytes per cell on disk and in memory — they are the same thing.
pub const CELL_MEM_BYTES: usize = 20;

/// `_pad[0]` bit: this cell's `ch` is an index into the grid's cluster
/// pool, not a codepoint.
///
/// A grapheme cluster of more than one codepoint — `é` written as
/// `e` + U+0301, a ZWJ family, a flag — used to lose everything after
/// its base.  Measured over 180 MB of real pane output, 55 clusters in
/// forty million are like this, so they go in a pool and the cell
/// holds a pointer to it rather than every cell paying for the case.
pub const FLAG_CLUSTER: u8 = 1 << 0;

/// Where pool indices start inside `char`.
///
/// Not zero.  `'\0'` already means "the second cell of a wide glyph"
/// here, and `' '` means blank, so an index that lands on one of them
/// is read as that by any code checking `ch` without checking the
/// flag — which is most of it, correctly, since before the pool there
/// was nothing else `ch` could be.  Plane 15 is private use: 65 536
/// slots, sixteen times the sweep threshold, and nothing in a real
/// stream is ever compared against these values.
pub const CLUSTER_INDEX_BASE: u32 = 0xF_0000;

impl Cell {
    /// Is this cell's `ch` a pool index rather than a character?
    #[inline]
    pub fn is_cluster(&self) -> bool {
        self.attrs._pad[0] & FLAG_CLUSTER != 0
    }

    /// The pool index this cell points at, if it points at one.
    #[inline]
    pub fn cluster_index(&self) -> Option<u32> {
        self.is_cluster()
            .then(|| (self.ch as u32).checked_sub(CLUSTER_INDEX_BASE))
            .flatten()
    }
}

const _: () = {
    assert!(std::mem::size_of::<Cell>() == CELL_MEM_BYTES);
    assert!(std::mem::align_of::<Cell>() == 4);
    assert!(std::mem::offset_of!(Cell, ch) == 0);
    assert!(std::mem::offset_of!(Cell, attrs) == 4);
    assert!(std::mem::size_of::<CellAttrs>() == 16);
    assert!(std::mem::offset_of!(CellAttrs, fg) == 0);
    assert!(std::mem::offset_of!(CellAttrs, bg) == 4);
    assert!(std::mem::offset_of!(CellAttrs, bold) == 8);
    assert!(std::mem::offset_of!(CellAttrs, italic) == 9);
    assert!(std::mem::offset_of!(CellAttrs, underline) == 10);
    assert!(std::mem::offset_of!(CellAttrs, reverse) == 11);
    assert!(std::mem::offset_of!(CellAttrs, dim) == 12);
    assert!(std::mem::offset_of!(CellAttrs, _pad) == 13);
    assert!(std::mem::size_of::<Color>() == 4);
    // The bytes are written native-endian; every build target is
    // little-endian Apple Silicon.  A big-endian build must not read
    // these files as if they matched.
    assert!(cfg!(target_endian = "little"));
};

impl Cell {
    /// A row of cells as the bytes they occupy.
    ///
    /// Every byte of a `Cell` is initialised: `char` and `bool` are
    /// fully-valued, `Color` zeroes the bytes a variant does not use
    /// (its constructors are the only way to make one), and
    /// `CellAttrs` spells its trailing padding out as a field.
    pub fn slice_as_bytes(cells: &[Cell]) -> &[u8] {
        // SAFETY: `Cell` is `repr(C)`, 20 bytes, with no implicit
        // padding (asserted above), and every field's bytes are
        // initialised for any value that can be constructed — see the
        // doc comment.  u8 has alignment 1 and the length is exact.
        unsafe {
            std::slice::from_raw_parts(cells.as_ptr() as *const u8, std::mem::size_of_val(cells))
        }
    }

    /// Read one cell back from its bytes.
    ///
    /// The bytes come from a file, so they are not trusted: an invalid
    /// `char` reads as a space, a bool byte is true for anything
    /// non-zero, and an unknown colour reads as the default — the same
    /// leniency the 13-byte decoder has always had.  Never a transmute.
    pub fn from_mem_bytes(b: &[u8; CELL_MEM_BYTES]) -> Cell {
        let ch = char::from_u32(u32::from_ne_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(' ');
        Cell {
            ch,
            attrs: CellAttrs {
                fg: Color::from_bytes([b[4], b[5], b[6], b[7]]),
                bg: Color::from_bytes([b[8], b[9], b[10], b[11]]),
                bold: b[12] != 0,
                italic: b[13] != 0,
                underline: b[14] != 0,
                reverse: b[15] != 0,
                dim: b[16] != 0,
                _pad: [0; 3],
            },
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            attrs: CellAttrs::default(),
        }
    }
}

impl From<char> for Cell {
    fn from(ch: char) -> Self {
        Cell {
            ch,
            attrs: CellAttrs::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct CellAttrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    /// SGR 2 — half-intensity / faint. Renderer dims the resolved fg
    /// (typical: multiply RGB by ~0.55). TUIs (claudecode tips column
    /// divider, dimmed help text) use this for "secondary" content.
    pub dim: bool,
    /// Flags that are not SGR, then two bytes that are always zero.
    ///
    /// The zeroes are still load-bearing: they are why the struct has
    /// no implicit padding and a `Cell` can go to disk as its own
    /// bytes — see `Cell`.  The first byte was one of them until a
    /// cluster needed somewhere to say it is a cluster; every value
    /// ever written to disk has it zero, which is exactly what
    /// `FLAG_CLUSTER` unset means.
    ///
    /// Public only because struct-update syntax (`..Default::default()`)
    /// cannot fill a private field from another crate.
    pub _pad: [u8; 3],
}

/// A cell colour: four bytes, tag then payload, unused bytes zero.
///
/// It was an enum (`Default` / `Indexed(u8)` / `Rgb(u8,u8,u8)`), and an
/// enum's variants leave the bytes they do not use uninitialised — two
/// of the three here.  That made a `Cell` impossible to hand to disk as
/// bytes without reading uninitialised memory.  As a plain struct built
/// only through the constructors below, the unused bytes are always
/// zero, so equal colours are equal byte for byte and every byte is
/// defined.  Matching goes through [`Color::kind`], which is still an
/// exhaustive enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Color {
    tag: u8,
    a: u8,
    b: u8,
    c: u8,
}

/// What a [`Color`] means, for matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorKind {
    /// "Use the default foreground/background" — the renderer picks the
    /// theme's default.  This is distinct from Indexed(0), which is the
    /// palette's first color.
    Default,
    /// Indexed palette entry.  0–7 = standard, 8–15 = bright variants,
    /// 16–255 = 256-color extended palette.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

impl Color {
    const TAG_DEFAULT: u8 = 0;
    const TAG_INDEXED: u8 = 1;
    const TAG_RGB: u8 = 2;

    pub const DEFAULT: Color = Color { tag: Self::TAG_DEFAULT, a: 0, b: 0, c: 0 };

    pub const fn indexed(i: u8) -> Color {
        Color { tag: Self::TAG_INDEXED, a: i, b: 0, c: 0 }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { tag: Self::TAG_RGB, a: r, b: g, c: b }
    }

    #[inline]
    pub const fn kind(self) -> ColorKind {
        match self.tag {
            Self::TAG_INDEXED => ColorKind::Indexed(self.a),
            Self::TAG_RGB => ColorKind::Rgb(self.a, self.b, self.c),
            // Only the constructors make a `Color`, so this is
            // `TAG_DEFAULT`; `from_bytes` maps anything else here too.
            _ => ColorKind::Default,
        }
    }

    /// From four stored bytes.  Canonicalises: an unknown tag is the
    /// default colour, and bytes a variant does not use are dropped, so
    /// the result always compares equal to the constructor's.
    pub const fn from_bytes(b: [u8; 4]) -> Color {
        match b[0] {
            Self::TAG_INDEXED => Color::indexed(b[1]),
            Self::TAG_RGB => Color::rgb(b[1], b[2], b[3]),
            _ => Color::DEFAULT,
        }
    }
}

impl From<ColorKind> for Color {
    fn from(k: ColorKind) -> Color {
        match k {
            ColorKind::Default => Color::DEFAULT,
            ColorKind::Indexed(i) => Color::indexed(i),
            ColorKind::Rgb(r, g, b) => Color::rgb(r, g, b),
        }
    }
}

#[cfg(test)]
mod cell_bytes_tests {
    use super::*;

    /// A cell written as its own bytes reads back as the same cell,
    /// for every colour kind and every attribute.
    #[test]
    fn a_cell_survives_its_own_bytes() {
        let colors = [Color::DEFAULT, Color::indexed(0), Color::indexed(255), Color::rgb(1, 2, 3)];
        let mut row = Vec::new();
        for (i, fg) in colors.iter().enumerate() {
            for bg in colors {
                row.push(Cell {
                    ch: ['x', '中', '😀', '\u{10FFFF}'][i],
                    attrs: CellAttrs {
                        fg: *fg,
                        bg,
                        bold: i % 2 == 0,
                        italic: i % 2 == 1,
                        underline: true,
                        reverse: false,
                        dim: i == 3,
                        ..Default::default()
                    },
                });
            }
        }
        let bytes = Cell::slice_as_bytes(&row);
        assert_eq!(bytes.len(), row.len() * CELL_MEM_BYTES);
        for (i, chunk) in bytes.chunks_exact(CELL_MEM_BYTES).enumerate() {
            assert_eq!(Cell::from_mem_bytes(chunk.try_into().unwrap()), row[i], "cell {i}");
        }
    }

    /// Bytes from a file are not trusted: nonsense reads as something
    /// valid and canonical, never as an invalid char or bool.
    #[test]
    fn garbage_bytes_read_as_a_valid_cell() {
        let mut b = [0xFFu8; CELL_MEM_BYTES];
        // 0xFFFFFFFF is not a char; tag 0xFF is not a colour.
        let c = Cell::from_mem_bytes(&b);
        assert_eq!(c.ch, ' ');
        assert_eq!(c.attrs.fg, Color::DEFAULT);
        assert!(c.attrs.bold && c.attrs.dim);
        assert_eq!(c.attrs._pad, [0; 3]);
        // An Indexed colour with junk in the bytes it does not use
        // still equals the canonical one.
        b[4..8].copy_from_slice(&[1, 7, 99, 99]);
        assert_eq!(Cell::from_mem_bytes(&b).attrs.fg, Color::indexed(7));
    }

    /// Equal colours are equal byte for byte — the property the disk
    /// format leans on — because the constructors zero what a kind
    /// does not use.
    #[test]
    fn colours_are_canonical() {
        assert_eq!(Color::from_bytes([1, 7, 0, 0]), Color::indexed(7));
        assert_eq!(Color::from_bytes([0, 9, 9, 9]), Color::DEFAULT);
        assert_eq!(Color::from(ColorKind::Rgb(4, 5, 6)), Color::rgb(4, 5, 6));
        assert_eq!(Color::rgb(4, 5, 6).kind(), ColorKind::Rgb(4, 5, 6));
        assert_eq!(Color::default(), Color::DEFAULT);
    }
}
