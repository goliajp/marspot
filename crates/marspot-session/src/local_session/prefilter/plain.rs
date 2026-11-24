//! Runs of plain text, which no text's match can begin or end in.

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// `b` repeated over a word.
pub(super) fn word(b: u8) -> u64 {
    ONES * u64::from(b)
}

/// One of the word's bytes is zero.
#[inline]
fn zero_byte(x: u64) -> bool {
    x.wrapping_sub(ONES) & !x & HIGHS != 0
}

/// Where the plain text from `i` on ends: the next control byte, or
/// the next first byte of a text, `firsts` as words and `stops` as
/// bytes.  Eight bytes at a time.
#[inline]
pub(super) fn end(firsts: &[u64], stops: &[bool; 256], bytes: &[u8], mut i: usize) -> usize {
    while let Some(w) = bytes.get(i..i + 8) {
        let x = u64::from_le_bytes(w.try_into().unwrap_or_default());
        let control = x.wrapping_sub(ONES * 0x20) & !x & HIGHS;
        if control != 0 || firsts.iter().any(|&f| zero_byte(x ^ f)) {
            break;
        }
        i += 8;
    }
    while i < bytes.len() && !stops[usize::from(bytes[i])] {
        i += 1;
    }
    i
}
