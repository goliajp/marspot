//! The text lane: a run of characters that can each only be a cluster
//! of their own is written straight into the row, without a trip
//! through the per-character path for each one.
//!
//! What it writes is by construction what `print` would have written
//! for the same characters one at a time; `Terminal::feed_reference`
//! is that path, and the differential tests hold the two together.

use super::Handler;
use crate::grid::Cell;
use crate::parser::scan::printable_run_len;

/// Raw codepoint of a 3-byte UTF-8 sequence (caller checked shape).
#[inline(always)]
fn decode3_cp(b: &[u8]) -> u32 {
    (((b[0] & 0x0F) as u32) << 12) | (((b[1] & 0x3F) as u32) << 6) | (b[2] & 0x3F) as u32
}

/// The fast-path class for `Handler::print`: codepoints that can
/// never open, extend, or join a multi-codepoint cluster — GBP=Other,
/// not Extended_Pictographic, InCB=None — with their cell width.  Two
/// neighbours from this class always have a cluster boundary between
/// them (UAX #29 GB999) and their width needs no table walk, so the
/// segmenter can be skipped entirely.
///
/// The ranges are hand-picked hot blocks (ASCII, CJK ideographs,
/// kana, CJK punctuation, fullwidth forms); every member is verified
/// against the real UCD tables by `fast_path_class_is_sound`, so a
/// table regen that invalidated one would fail the suite before it
/// could mis-render.  Deliberately NOT in the class: Hangul
/// syllables (GBP LV/LVT, GB6-8), anything Extended_Pictographic
/// (a following VS16/ZWJ changes width — early commit would be
/// wrong), combining kana marks U+3099/309A (GBP Extend).
#[inline(always)]
pub(super) fn boring_width(ch: char) -> Option<u8> {
    match ch as u32 {
        0x20..=0x7E => Some(1),     // ASCII printable
        0x4E00..=0x9FFF => Some(2), // CJK Unified Ideographs
        0x3041..=0x3096 => Some(2), // hiragana (sans 3099/309A marks)
        0x30A1..=0x30FA => Some(2), // katakana
        0x30FC..=0x30FE => Some(2), // ー ヽ ヾ (sans 30FF)
        0x3001..=0x3029 => Some(2), // CJK punctuation 、。「」等
        0xFF01..=0xFF60 => Some(2), // fullwidth forms
        0x3400..=0x4DBF => Some(2), // CJK Extension A
        _ => None,
    }
}

/// `boring_width` plus precomposed Hangul syllables.  Syllables are
/// GBP LV/LVT — they can conjoin with FOLLOWING jamo (GB7/GB8), so
/// they don't belong in `boring_width`'s "never interacts" story,
/// but every pair drawn from {Other, LV, LVT} still has an
/// unconditional boundary between them (GB6-8 only join when the
/// NEXT char is jamo; GB9b needs a Prepend prev; neither class is
/// in here).  Combined with the invariant that a fast commit always
/// leaves the LAST char as the open cluster (so a following jamo, VS,
/// or mark meets it via the slow path), that makes this the widest
/// class the fast paths may commit early.
#[inline(always)]
pub(super) fn fast_width(ch: char) -> Option<u8> {
    if let Some(w) = boring_width(ch) {
        return Some(w);
    }
    if matches!(ch as u32, 0xAC00..=0xD7A3) {
        return Some(2); // Hangul syllables (LV / LVT)
    }
    fast_pict_width(ch)
}

/// A pictograph that can only ever be a cluster of its own — the
/// emoji half of the fast class.
///
/// Admitting one is the same bargain `fast_width`'s doc describes:
/// the pair (this, anything in the fast class) has an unconditional
/// UAX #29 boundary, and the last character of a fast commit always
/// stays the open cluster, so a VS16 / ZWJ / skin-tone modifier
/// arriving next still meets it on the slow path.  What must be
/// excluded is anything whose boundary depends on a neighbour:
///
///   - Regional indicators (GB12/13 pair into flags: 🇯🇵 is ONE cluster)
///   - Any codepoint that is not GBP=Other (Extend / ZWJ /
///     SpacingMark / Prepend all join across the boundary)
///
/// Width comes from the same `has_emoji_presentation` table
/// `char_width` consults, so the fast and slow paths cannot disagree
/// about how many cells the glyph takes.  A text-presentation
/// pictograph (⚠ without VS16) is deliberately NOT admitted: its
/// width depends on a variation selector that may still be coming.
#[inline]
pub(super) fn fast_pict_width(ch: char) -> Option<u8> {
    let cp = ch as u32;
    // Below the symbol blocks nothing has emoji presentation, and
    // this is the branch every ASCII / Latin / CJK character takes.
    if cp < 0x2190 {
        return None;
    }
    // The two exclusions are ranges rather than a `gbp()` call: across
    // the whole codepoint space the only emoji-presentation characters
    // that are NOT GBP=Other are these two blocks.
    // `the_fast_pictograph_class_never_outruns_the_tables` re-derives
    // that from the tables themselves, so a Unicode update that adds a
    // third one fails the build instead of silently splitting a
    // cluster.
    if (0x1F1E6..=0x1F1FF).contains(&cp)     // regional indicator — GB12/13 pairs it
        || (0x1F3FB..=0x1F3FF).contains(&cp)
    // skin-tone modifier — GBP=Extend
    {
        return None;
    }
    if !crate::emoji_presentation::has_emoji_presentation(cp) {
        return None;
    }
    Some(2)
}

/// A multi-byte character at the head of `b` that takes two cells and
/// is of the fast class, with its encoded length.
///
/// The shape test is the parser's own: a lead byte and continuation
/// bytes, nothing more.  An overlong or out-of-range encoding decodes
/// to something that is not a two-cell fast-class scalar and is left
/// to the per-byte path, which prints whatever the parser makes of it.
#[inline(always)]
fn wide_at(b: &[u8]) -> Option<(char, usize)> {
    let b0 = *b.first()?;
    if (0xE0..=0xEF).contains(&b0) {
        if b.len() < 3 || b[1] & 0xC0 != 0x80 || b[2] & 0xC0 != 0x80 {
            return None;
        }
        let c = char::from_u32(decode3_cp(b))?;
        (fast_width(c) == Some(2)).then_some((c, 3))
    } else if (0xF0..=0xF4).contains(&b0) {
        if b.len() < 4 || b[1] & 0xC0 != 0x80 || b[2] & 0xC0 != 0x80 || b[3] & 0xC0 != 0x80 {
            return None;
        }
        let cp = (((b0 & 0x07) as u32) << 18)
            | (((b[1] & 0x3F) as u32) << 12)
            | (((b[2] & 0x3F) as u32) << 6)
            | (b[3] & 0x3F) as u32;
        // nothing past the BMP is in the class but pictographs
        let c = char::from_u32(cp)?;
        (fast_pict_width(c) == Some(2)).then_some((c, 4))
    } else {
        None
    }
}

impl<'a> Handler<'a> {
    /// End the pending cluster ahead of a fast-class head IF a
    /// boundary before that head is unconditional.  Returns false when
    /// the cluster could legally absorb it — GB9b (ends in a Prepend)
    /// for any head, GB6-8 (ends in jamo / syllable) for a Hangul
    /// syllable, GB11 (ends in ZWJ) for a pictograph — and the caller
    /// must leave the head to the per-character path.
    fn lane_prologue(&mut self, head: char) -> bool {
        if self.cluster_buf.is_empty() || self.cluster_fast.is_some() {
            // nothing open, or a fast-class character: `print` would
            // commit the head without asking the segmenter either
            return true;
        }
        let mut it = self.cluster_buf.chars();
        let first = it.next().expect("non-empty buffer");
        if it.next().is_none() && fast_width(first).is_some() {
            self.flush_cluster_keep_cursor();
            return true;
        }
        use crate::unicode_data::{GBP, gbp};
        let last = self.cluster_buf.chars().next_back().expect("non-empty");
        let lp = gbp(last as u32);
        if lp == GBP::Prepend {
            return false;
        }
        let cp = head as u32;
        if matches!(cp, 0xAC00..=0xD7A3)
            && matches!(lp, GBP::L | GBP::V | GBP::T | GBP::LV | GBP::LVT)
        {
            return false;
        }
        if lp == GBP::ZWJ && cp >= 0x2190 && boring_width(head).is_none() {
            return false;
        }
        self.flush_cluster_keep_cursor();
        true
    }

    /// Write the longest fast-class prefix of `bytes` and return its
    /// length in bytes; 0 when the head is not one this lane takes.
    ///
    /// The caller has the parser in plain Ground.  Each character
    /// lands where `write_glyph` would have put it: a deferred wrap is
    /// taken before the next glyph, a wide glyph that does not fit in
    /// the last column pads it and wraps, and a run that ends on the
    /// right edge leaves the wrap deferred.  The last character is
    /// left as the open cluster, anchored where it was written, so a
    /// mark or selector arriving next amends it.
    pub(super) fn text_lane(&mut self, bytes: &[u8]) -> usize {
        // with G0 on the special graphics set the bytes are not the
        // characters to write, and under IRM each one has to push the
        // rest of the line along first
        if *self.g0_graphics || *self.insert_mode {
            return 0;
        }
        // a tag being recognised owns every character until it is
        // settled, and `<` is where one starts
        let tags = self.u_tags;
        if tags && (*self.u_open || !self.u_buf.is_empty()) {
            return 0;
        }
        let cols = self.grid.cols() as usize;
        if cols < 2 {
            return 0;
        }
        let rows = self.grid.rows();
        let printable = |b: &[u8]| {
            let n = printable_run_len(b);
            if tags {
                b[..n].iter().position(|&c| c == b'<').unwrap_or(n)
            } else {
                n
            }
        };
        // the head decides whether the lane applies at all, and
        // nothing is touched until it has
        let head = match bytes.first() {
            Some(&b) if b.wrapping_sub(0x20) < 0x5F => {
                if tags && b == b'<' {
                    return 0;
                }
                b as char
            }
            _ => match wide_at(bytes) {
                Some((c, _)) => c,
                None => return 0,
            },
        };
        if !self.lane_prologue(head) {
            return 0;
        }
        if *self.pending_wrap {
            *self.pending_wrap = false;
            let (_, row) = self.grid.cursor();
            self.wrap_to_next_row(row, rows);
        }
        let attrs = *self.attrs;
        let mut idx = 0;
        // the last character written: (char, width, column), in `row`
        let mut last = (head, 1u8, 0u16);
        let row = loop {
            let (col0, row) = self.grid.cursor();
            let mut col = col0 as usize;
            let cells = self.grid.row_cells_mut(0, row, cols);
            // why the row was left: 0 the run ended, 1 the row is
            // full, 2 a wide glyph is next and one column is left
            let left = loop {
                let rest = &bytes[idx..];
                let Some(&b0) = rest.first() else { break 0 };
                if b0.wrapping_sub(0x20) < 0x5F {
                    let n = printable(rest);
                    if n == 0 {
                        break 0;
                    }
                    let m = n.min(cols - col);
                    if m == 0 {
                        break 1;
                    }
                    for (cell, &b) in cells[col..col + m].iter_mut().zip(rest) {
                        *cell = Cell { ch: b as char, attrs };
                    }
                    col += m;
                    idx += m;
                    last = (rest[m - 1] as char, 1, (col - 1) as u16);
                } else if let Some((c, len)) = wide_at(rest) {
                    if col + 2 > cols {
                        break if col == cols { 1 } else { 2 };
                    }
                    cells[col] = Cell { ch: c, attrs };
                    cells[col + 1] = Cell { ch: '\0', attrs };
                    last = (c, 2, col as u16);
                    col += 2;
                    idx += len;
                } else {
                    break 0;
                }
            };
            if left == 0 {
                if col < cols {
                    self.grid.set_cursor(col as u16, row);
                } else {
                    self.grid.set_cursor((cols - 1) as u16, row);
                    *self.pending_wrap = true;
                }
                break row;
            }
            if left == 2 && cells[cols - 1] == Cell::default() {
                // the abandoned last column is marked as padding, not
                // a space someone typed
                cells[cols - 1] = Cell { ch: '\0', attrs };
            }
            self.wrap_to_next_row(row, rows);
        };
        let (ch, w, col) = last;
        self.cluster_buf.clear();
        self.cluster_buf.push(ch);
        *self.cluster_fast = Some((ch, w));
        *self.cluster_anchor = Some((col, row, w));
        *self.seg_synced = false;
        idx
    }
}
