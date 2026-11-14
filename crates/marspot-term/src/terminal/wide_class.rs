//! The two-cell half of the fast class as bit tables, for the text
//! lane's decode: one load and a shift where `fast_width` walks its
//! ranges.  `fast_width` stays the definition; the test below holds
//! the tables to it for every scalar value.

use crate::emoji_presentation::EMOJI_PRESENTATION_RANGES;

/// The ranges of `boring_width` and `fast_width` that are two cells
/// wide, below the supplementary planes.
const WIDE_RANGES: [(u32, u32); 8] = [
    (0x4E00, 0x9FFF),
    (0x3041, 0x3096),
    (0x30A1, 0x30FA),
    (0x30FC, 0x30FE),
    (0x3001, 0x3029),
    (0xFF01, 0xFF60),
    (0x3400, 0x4DBF),
    (0xAC00, 0xD7A3),
];

/// Where the supplementary table starts and how many code points it
/// covers: every pictograph `fast_pict_width` admits is inside.
const SMP_LO: u32 = 0x1F000;
const SMP_LEN: u32 = 0xB00;

const fn set(table: &mut [u64], lo: u32, start: u32, end: u32) {
    let mut cp = start;
    while cp <= end {
        let off = (cp - lo) as usize;
        if off / 64 < table.len() {
            table[off / 64] |= 1 << (off % 64);
        }
        cp += 1;
    }
}

const fn bmp_table() -> [u64; 1024] {
    let mut t = [0u64; 1024];
    let mut i = 0;
    while i < WIDE_RANGES.len() {
        set(&mut t, 0, WIDE_RANGES[i].0, WIDE_RANGES[i].1);
        i += 1;
    }
    let mut i = 0;
    while i < EMOJI_PRESENTATION_RANGES.len() {
        let (start, end) = EMOJI_PRESENTATION_RANGES[i];
        // `fast_pict_width` admits nothing below the symbol blocks
        if start >= 0x2190 && end <= 0xFFFF {
            set(&mut t, 0, start, end);
        }
        i += 1;
    }
    t
}

const fn smp_table() -> [u64; (SMP_LEN / 64) as usize] {
    let mut t = [0u64; (SMP_LEN / 64) as usize];
    let mut i = 0;
    while i < EMOJI_PRESENTATION_RANGES.len() {
        let (start, end) = EMOJI_PRESENTATION_RANGES[i];
        if start >= SMP_LO && end < SMP_LO + SMP_LEN {
            let mut cp = start;
            while cp <= end {
                // regional indicators pair into flags and skin tones
                // extend what they follow: neither stands alone
                let apart = !(cp >= 0x1F1E6 && cp <= 0x1F1FF) && !(cp >= 0x1F3FB && cp <= 0x1F3FF);
                if apart {
                    set(&mut t, SMP_LO, cp, cp);
                }
                cp += 1;
            }
        }
        i += 1;
    }
    t
}

static BMP_WIDE: [u64; 1024] = bmp_table();
static SMP_WIDE: [u64; (SMP_LEN / 64) as usize] = smp_table();

/// A multi-byte character at the head of `b` that takes two cells and
/// is of the fast class, with its encoded length.
///
/// The shape test is the parser's own: a lead byte and continuation
/// bytes, nothing more.  An overlong or out-of-range encoding decodes
/// to something that is not a two-cell fast-class scalar and is left
/// to the per-byte path, which prints whatever the parser makes of it.
#[inline(always)]
pub(super) fn wide_at(b: &[u8]) -> Option<(char, usize)> {
    let b0 = *b.first()?;
    if (0xE0..=0xEF).contains(&b0) {
        if b.len() < 3 || b[1] & 0xC0 != 0x80 || b[2] & 0xC0 != 0x80 {
            return None;
        }
        let cp = (((b0 & 0x0F) as u32) << 12) | (((b[1] & 0x3F) as u32) << 6) | (b[2] & 0x3F) as u32;
        if BMP_WIDE[(cp >> 6) as usize & 1023] >> (cp & 63) & 1 == 0 {
            return None;
        }
        Some((char::from_u32(cp)?, 3))
    } else if (0xF0..=0xF4).contains(&b0) {
        if b.len() < 4 || b[1] & 0xC0 != 0x80 || b[2] & 0xC0 != 0x80 || b[3] & 0xC0 != 0x80 {
            return None;
        }
        let cp = (((b0 & 0x07) as u32) << 18)
            | (((b[1] & 0x3F) as u32) << 12)
            | (((b[2] & 0x3F) as u32) << 6)
            | (b[3] & 0x3F) as u32;
        let off = cp.wrapping_sub(SMP_LO);
        if off >= SMP_LEN || SMP_WIDE[(off >> 6) as usize] >> (off & 63) & 1 == 0 {
            return None;
        }
        Some((char::from_u32(cp)?, 4))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::lane::{fast_pict_width, fast_width};
    use super::wide_at;

    /// Every scalar value, encoded, is taken by `wide_at` exactly when
    /// the class functions call it a two-cell fast-class character:
    /// `fast_width` for three bytes, `fast_pict_width` for four.
    #[test]
    fn the_tables_say_what_the_class_functions_say() {
        for cp in 0x80..=0x10FFFFu32 {
            let Some(ch) = char::from_u32(cp) else { continue };
            let mut buf = [0u8; 4];
            let len = ch.encode_utf8(&mut buf).len();
            let want = match len {
                3 => fast_width(ch) == Some(2),
                4 => fast_pict_width(ch) == Some(2),
                _ => false,
            };
            let got = wide_at(&buf[..len]);
            assert_eq!(got, want.then_some((ch, len)), "U+{cp:04X}");
        }
    }

    /// Bytes shaped like a character that are not one: surrogates,
    /// overlong forms, values past the last plane, a sequence cut short.
    #[test]
    fn what_is_not_a_character_is_not_taken() {
        for raw in [
            &b"\xed\xa0\x80"[..],
            b"\xed\xbf\xbf",
            b"\xe0\x80\xa0",
            b"\xe0\x9f\xbf",
            b"\xf0\x80\x80\x80",
            b"\xf0\x8f\xbf\xbf",
            b"\xf4\x90\x80\x80",
            b"\xf5\x80\x80\x80",
            b"\xe4\xb8",
            b"\xf0\x9f\x9a",
            b"\xe4\x41\xad",
            b"\xf0\x9f\x41\x80",
        ] {
            assert_eq!(wide_at(raw), None, "{raw:02x?}");
        }
        // every overlong three- and four-byte form
        for cp in 0..0x800u32 {
            let b = [0xE0, 0x80 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8];
            assert_eq!(wide_at(&b), None, "overlong U+{cp:04X}");
        }
        for cp in (0..0x10000u32).step_by(7) {
            let b = [0xF0, 0x80 | (cp >> 12) as u8, 0x80 | ((cp >> 6) & 0x3F) as u8, 0x80 | (cp & 0x3F) as u8];
            assert_eq!(wide_at(&b), None, "overlong U+{cp:04X}");
        }
    }
}
