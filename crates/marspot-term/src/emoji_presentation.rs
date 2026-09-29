//! Generated from the Unicode emoji-data.txt (UTS #51) — every
//! codepoint with `Emoji_Presentation=Yes`.  These are the code
//! points that default to emoji (color, em-box-wide) presentation,
//! the canonical definition of "is this character an emoji" for
//! the terminal-width purpose.  Characters with Emoji=Yes but
//! Emoji_Presentation=No (e.g. ⚠ U+26A0, ☝ U+261D) default to TEXT
//! presentation — they become emoji only when followed by VS16
//! (U+FE0F), which the grapheme cluster pass handles separately.
//!
//! Source: https://www.unicode.org/Public/UCD/latest/ucd/emoji/emoji-data.txt
//! Version: 17.0
//!
//! Regeneration: bin/regen-unicode-tables.sh.

/// Sorted, non-overlapping inclusive ranges `(start, end)` of
/// codepoints with `Emoji_Presentation=Yes`.
pub const EMOJI_PRESENTATION_RANGES: &[(u32, u32)] = &[
    (0x231A, 0x231B),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F1E6, 0x1F1FF),
    (0x1F201, 0x1F201),
    (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F236),
    (0x1F238, 0x1F23A),
    (0x1F250, 0x1F251),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D8),
    (0x1F6DC, 0x1F6DF),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA8A),
    (0x1FA8E, 0x1FAC6),
    (0x1FAC8, 0x1FAC8),
    (0x1FACD, 0x1FADC),
    (0x1FADF, 0x1FAEA),
    (0x1FAEF, 0x1FAF8),
];

/// `true` iff `cp` is an Emoji_Presentation=Yes codepoint per UTS #51.
// The ranges above are the source of truth; these bitmaps are built
// from them at compile time so there is still only one table to
// regenerate.  The lookup they replace was a binary search over 81
// ranges — ~7 unpredictable branches for every emoji that reaches the
// width fast path, on a path where one added branch per character
// measurably moves throughput.  1219 codepoints are set across two
// spans, which fit in 615 bytes of rodata.
const BMP_LO: u32 = 0x231A;
const BMP_HI: u32 = 0x2B55;
const SMP_LO: u32 = 0x1F004;
const SMP_HI: u32 = 0x1FAF8;
const BMP_BYTES: usize = (BMP_HI - BMP_LO) as usize / 8 + 1;
const SMP_BYTES: usize = (SMP_HI - SMP_LO) as usize / 8 + 1;

const fn bitmap<const N: usize>(lo: u32, hi: u32) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < EMOJI_PRESENTATION_RANGES.len() {
        let (start, end) = EMOJI_PRESENTATION_RANGES[i];
        let mut cp = if start < lo { lo } else { start };
        let last = if end > hi { hi } else { end };
        while cp <= last {
            let off = (cp - lo) as usize;
            out[off / 8] |= 1 << (off % 8);
            cp += 1;
        }
        i += 1;
    }
    out
}

static BMP: [u8; BMP_BYTES] = bitmap::<BMP_BYTES>(BMP_LO, BMP_HI);
static SMP: [u8; SMP_BYTES] = bitmap::<SMP_BYTES>(SMP_LO, SMP_HI);

#[inline]
fn bit(map: &[u8], lo: u32, cp: u32) -> bool {
    let off = (cp - lo) as usize;
    map[off / 8] & (1 << (off % 8)) != 0
}

pub fn has_emoji_presentation(cp: u32) -> bool {
    if cp >= SMP_LO {
        cp <= SMP_HI && bit(&SMP, SMP_LO, cp)
    } else {
        cp >= BMP_LO && cp <= BMP_HI && bit(&BMP, BMP_LO, cp)
    }
}

/// The binary search the bitmaps replaced.  Kept as the reference the
/// equivalence test checks against — a table regeneration that widens
/// a span past the bitmap bounds has to fail a test, not silently
/// start answering `false`.
#[cfg(test)]
fn has_emoji_presentation_by_search(cp: u32) -> bool {
    EMOJI_PRESENTATION_RANGES
        .binary_search_by(|&(start, end)| {
            if cp < start {
                std::cmp::Ordering::Greater
            } else if cp > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_bitmap_answers_exactly_what_the_ranges_say() {
        // Every codepoint, not a sample: the bitmaps are bounded by
        // hand-written spans, and a regenerated table that grows past
        // one of them would otherwise answer `false` in silence.
        for cp in 0..=0x10FFFFu32 {
            assert_eq!(
                has_emoji_presentation(cp),
                has_emoji_presentation_by_search(cp),
                "U+{cp:04X}"
            );
        }
    }

    #[test]
    fn known_presentation_emoji_match_spec() {
        assert!(has_emoji_presentation(0x2705), "check mark");
        assert!(has_emoji_presentation(0x274C), "cross mark");
        assert!(has_emoji_presentation(0x2B50), "star");
        assert!(has_emoji_presentation(0x1F33F), "herb");
        assert!(!has_emoji_presentation(0x26A0), "warning (text default)");
        assert!(!has_emoji_presentation(0x261D), "index up (text default)");
        assert!(!has_emoji_presentation(b"A"[0] as u32));
        assert!(!has_emoji_presentation(0x4E2D), "CJK middle");
    }
    #[test]
    fn ranges_sorted_and_disjoint() {
        let mut prev_end = 0u32;
        for (i, &(s, e)) in EMOJI_PRESENTATION_RANGES.iter().enumerate() {
            assert!(s <= e, "range[{}] start > end", i);
            if i > 0 {
                assert!(prev_end < s, "range[{}] overlaps", i);
            }
            prev_end = e;
        }
    }
}
