//! A cell's attributes packed into eight bytes.

use super::{CellAttrs, Color};

/// A cell's attributes in eight bytes; zero is the default.
///
/// Byte 0: the foreground's tag in bits 0–1, then bold, italic,
/// underline, reverse and dim in bits 2–6; bit 7 is always clear.
/// Bytes 1–3: the foreground's payload.  Byte 4: the background's tag;
/// bytes 5–7 its payload.  Every attribute set packs to exactly one
/// value and unpacks to the one it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct PackedStyle(pub u64);

pub(super) const FLAGS_SHIFT: u32 = 2;
pub(super) const FLAGS_MASK: u64 = 0x1F << FLAGS_SHIFT;
/// SGR 8 and SGR 5, in the background's tag byte.  Never in a frame:
/// the writer takes them out of the values it publishes, so readers
/// that predate them see only what they know.
pub const INVISIBLE: u64 = 1 << 34;
pub const BLINK: u64 = 1 << 35;
const UNSHOWN: u64 = INVISIBLE | BLINK;
/// Set only on a table slot that is free; no packed value has it.
pub(super) const FREE: u64 = 1 << 7;

impl PackedStyle {
    pub const DEFAULT: PackedStyle = PackedStyle(0);

    #[inline(always)]
    pub fn pack(a: &CellAttrs) -> PackedStyle {
        let words = a as *const CellAttrs as *const u64;
        // SAFETY: `CellAttrs` is `repr(C)`, sixteen bytes, its padding
        // spelt out as a field and every byte initialised (asserted
        // beside `Cell`); the reads cover exactly those bytes and make
        // no claim about alignment.
        let (colours, bools) = unsafe { (words.read_unaligned(), words.add(1).read_unaligned()) };
        // the two colours are the first eight bytes as they stand; the
        // five bools (bytes of 0 or 1) gather into five bits with one
        // multiply, each landing on its own bit with nothing carried
        let flags = ((bools & 0x01_0101_0101).wrapping_mul(0x1_0204_0810) >> 32) & 0x1F;
        // `FLAG_INVISIBLE` and `FLAG_BLINK`, bits 1-2 of the padding
        let unshown = (bools >> 41 & 3) << 34;
        PackedStyle(colours | flags << FLAGS_SHIFT | unshown)
    }

    /// The sixteen bytes a `CellAttrs` holding this value has in
    /// memory, with `cluster` in the first byte of its padding.
    #[inline(always)]
    pub fn mem_bytes(self, cluster: bool) -> [u8; 16] {
        let colours = self.0 & !FLAGS_MASK & !UNSHOWN;
        let flags = (self.0 & FLAGS_MASK) >> FLAGS_SHIFT;
        // the inverse spread: bit i to byte i
        let bools = (flags.wrapping_mul(0x1020_4081) & 0x01_0101_0101)
            | (cluster as u64) << 40
            | (self.0 >> 34 & 3) << 41;
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&colours.to_le_bytes());
        b[8..].copy_from_slice(&bools.to_le_bytes());
        b
    }

    #[inline(always)]
    pub fn unpack(self) -> CellAttrs {
        let p = self.0;
        let flags = (p & FLAGS_MASK) >> FLAGS_SHIFT;
        CellAttrs {
            fg: Color::from_word(p as u32 & !(FLAGS_MASK as u32)),
            bg: Color::from_word((p >> 32) as u32 & !(UNSHOWN >> 32) as u32),
            bold: flags & 1 != 0,
            italic: flags & 2 != 0,
            underline: flags & 4 != 0,
            reverse: flags & 8 != 0,
            dim: flags & 16 != 0,
            _pad: [((p & UNSHOWN) >> 33) as u8, 0, 0],
        }
    }

    /// The same value without SGR 8 and 5: what a frame carries.
    #[inline(always)]
    pub fn shown(self) -> PackedStyle {
        PackedStyle(self.0 & !UNSHOWN)
    }

    #[inline(always)]
    pub fn has_unshown(self) -> bool {
        self.0 & UNSHOWN != 0
    }

    #[inline(always)]
    pub fn invisible(self) -> bool {
        self.0 & INVISIBLE != 0
    }

    #[inline(always)]
    pub fn is_default(self) -> bool {
        self.0 == 0
    }

    /// Whether [`Self::pack`] can produce this value.  For values that
    /// came from another process.
    pub fn is_valid(self) -> bool {
        // one colour as its four bytes: a tag of the three, and nothing
        // in the bytes that tag does not use
        let colour = |w: u32| match w & 0xFF {
            0 => w >> 8 == 0,
            1 => w >> 16 == 0,
            2 => true,
            _ => false,
        };
        let fg = self.0 as u32 & !(FLAGS_MASK as u32);
        self.0 & FREE == 0 && colour(fg) && colour((self.0 >> 32) as u32)
    }
}
