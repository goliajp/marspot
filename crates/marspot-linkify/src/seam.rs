//! Where a merged line may be cut again: wrap seams and the boundaries a URL or path stops at.

use crate::line::LineSegment;
use crate::oracle::PathOracle;
use crate::path::{is_real_path, looks_like_path};
use crate::sink::{TextBufs, collect_into};
use crate::wrap::is_url_path_class;

/// First zero-indent cc boundary strictly inside `(lo, hi)`, if
/// any.  Segments are ascending, so this returns the earliest one.
pub(crate) fn first_zero_indent_boundary(
    segments: &[LineSegment],
    lo: usize,
    hi: usize,
) -> Option<usize> {
    segments
        .iter()
        .filter(|s| s.cc_zero_indent)
        .map(|s| s.char_offset)
        .find(|&b| b > lo && b < hi)
}

/// The first zero-indent cc boundary inside `(lo, hi)` that a URL has
/// to stop at.
///
/// The default is to stop: a flush join is the weakest signal there
/// is, and a URL has no `stat` to arbitrate a wrong one.  The
/// exception is a break the renderer had no choice about — if the
/// first word of the row below would not have fitted in what was left
/// of the row above, the break is a wrap and the URL runs through it.
///
/// codex wraps every continuation flush at column 0, so without this
/// its URLs are cut at the seam: `https://` alone stops being a link
/// at all, and `https://chatgpt.com/backend-` keeps being one —
/// pointing somewhere other than the
/// `https://chatgpt.com/backend-api/codex/responses` on screen
/// (2026-09-26 report).
pub(crate) fn url_stop_boundary(
    chars: &[char],
    col_map: &[u16],
    segments: &[LineSegment],
    cols_per_row: usize,
    lo: usize,
    hi: usize,
) -> Option<usize> {
    segments
        .iter()
        .filter(|s| s.cc_zero_indent)
        .map(|s| s.char_offset)
        .filter(|&b| b > lo && b < hi)
        .find(|&b| !wrap_was_forced(chars, col_map, cols_per_row, b))
}

/// Did the row above the seam at `b` break because the next word did
/// not fit on it?
///
/// This is the measurement the renderer itself made, read back off the
/// screen — it assumes nothing about where a TUI's content width sits,
/// which is what every geometric guess in this file has had to be
/// widened for in turn.  When the word WOULD have fitted, the break is
/// one the program meant, and joining the rows would invent a link
/// that is on neither of them.
///
/// A row with no column left over answers nothing: a line can end
/// exactly at the edge by coincidence as easily as by wrapping, and
/// there is no room to say which.  The evidence is space the renderer
/// left unused — it would have used it if the next word had fitted.
///
/// Counts chars, not columns: a wide char is two columns, so a word
/// carrying one is measured short and the seam is treated as a real
/// newline.  That is the safe direction — a URL with a CJK char in its
/// first wrapped word loses nothing it had before this.
fn wrap_was_forced(chars: &[char], col_map: &[u16], cols_per_row: usize, b: usize) -> bool {
    if b == 0 || b >= chars.len() || b > col_map.len() {
        return false;
    }
    // Trailing blanks are popped at the seam, so this is the last
    // content column of the row above.
    let last_col = col_map[b - 1] as usize;
    if last_col + 1 >= cols_per_row {
        return false;
    }
    let word = chars[b..]
        .iter()
        .position(|c| *c == ' ')
        .unwrap_or(chars.len() - b);
    last_col + 1 + word > cols_per_row
}

/// A seam that cuts a name in half rather than ending one.
///
/// The row above stops at a `/` — a name does not end on a separator,
/// that is where one continues — and the row below opens with more
/// name.  Taking the seam as the end then draws a link over half of
/// what is on screen, pointing at a directory the line never named:
/// the 2026-09-27 report is `file:///…/goliajp/` on one row and
/// `vis/brand-dist/index-ja.html#principles` on the next, linked as
/// far as `goliajp/`.
///
/// Half a name is worse than none — 「现在的问题是识别一半，你要么完全
/// 不识别也好」— so the candidate is refused, and with nothing longer
/// on disk nothing is emitted at all.
///
/// Both ends of the seam have to say so, which is what keeps this from
/// swallowing the seam's real job:
///
/// * a row ending in a complete name with a separate token below it
///   (`…/docs/probe.sh` ⏎ `-n 5`) ends on `h`, not on a separator;
/// * a row below that opens with `/` starts an absolute path of its
///   own — a listing of directories, one per row (`…/bb/` ⏎
///   `/Users/…/ccc/`), is three complete names, not one broken one.
pub(crate) fn seam_cuts_a_name(chars: &[char], cut: usize) -> bool {
    cut > 0
        && cut < chars.len()
        && chars[cut - 1] == '/'
        && chars[cut] != '/'
        && is_url_path_class(chars[cut])
}

/// The cc hard-wrap merge is a heuristic guess.  When a merged
/// filesystem-path candidate doesn't stat, the glue may have absorbed
/// the FOLLOWING row's first word: the path ended flush at the right
/// edge (which looks exactly like a claudecode hard wrap) and the
/// next row's opening word happens to be path-class — e.g.
/// `.../feedback.md` + next row `fullpath 已交` merges into
/// `.../feedback.mdfullpath`, and the real, existing single-row path
/// is silently lost.  Retry the prefixes that end exactly at a
/// segment boundary, longest first; the first one that exists on
/// disk is the real token.  Returns the char-pos to truncate at.
/// (URL candidates have no existence oracle, so they keep the plain
/// merged behaviour.)
pub(crate) fn retry_file_at_segment_boundaries(
    chars: &[char],
    segments: &[LineSegment],
    lo: usize,
    hi: usize,
    oracle: &dyn PathOracle,
    bufs: &mut TextBufs,
) -> Option<usize> {
    for seg in segments.iter().rev() {
        let b = seg.char_offset;
        if b <= lo || b >= hi {
            continue;
        }
        if seam_cuts_a_name(chars, b) {
            continue;
        }
        let prefix = &chars[lo..b];
        if !looks_like_path(prefix) {
            continue;
        }
        collect_into(&mut bufs.span, prefix);
        if is_real_path(oracle, &bufs.span) {
            return Some(b);
        }
    }
    None
}
