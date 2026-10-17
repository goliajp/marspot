//! The fixed-width TUI hard-wrap heuristic: does a row continue the token the row above was cut in.

use crate::CellSource;

/// cc hard-wrap heuristic: does `r` look like a continuation of the
/// URL/path that the previous row was rendering?  A false negative
/// leaves the URL split as today.  A false positive is USUALLY
/// harmless (the pattern scan just won't match) — except for
/// filesystem paths, where the glued next-row word breaks the stat
/// check on an otherwise-valid single-row path;
/// `retry_file_at_segment_boundaries` recovers that case at emit
/// time.
///
///   - previous row's last non-blank cell column ≥ cols - 2 (touches
///     or near the right edge — claudecode hard-wraps flush right)
///   - that last non-blank cell's char is URL/path-class
///   - current row has 0..=4 leading whitespace cells; ZERO indent
///     (claudecode's input box char-wraps long tokens mid-word with
///     no hanging indent) additionally requires the previous row to
///     be COMPLETELY full — a mid-word char wrap always occupies the
///     last column
///   - current row's first non-blank cell's char is URL/path-class
pub(crate) fn is_hard_wrap_continuation<S: CellSource>(
    src: &S,
    prev_row: u16,
    curr_row: u16,
    cols: u16,
) -> bool {
    // The whole row is the band: content spans 0..cols.
    is_hard_wrap_continuation_in(src, prev_row, curr_row, 0, cols)
}

/// The same heuristic over an arbitrary column band `left..right`.
///
/// A table cell is a band whose edges are the cell's borders rather
/// than the pane's, and everything the heuristic asks — "did the
/// previous row run out of room?", "does this row take up where it
/// left off?" — is asked of those edges instead.  The full-row form
/// above is the `0..cols` case of exactly this.
pub(crate) fn is_hard_wrap_continuation_in<S: CellSource>(
    src: &S,
    prev_row: u16,
    curr_row: u16,
    left: u16,
    right: u16,
) -> bool {
    if right < left + 2 {
        return false;
    }
    let cols = right;
    let mut last_nb_col: Option<u16> = None;
    let mut last_nb_ch = ' ';
    for c in (left..right).rev() {
        let ch = src.char_at(c, prev_row);
        if ch != '\0' && ch != ' ' {
            last_nb_col = Some(c);
            last_nb_ch = ch;
            break;
        }
    }
    let last_nb_col = match last_nb_col {
        Some(c) => c,
        None => return false,
    };
    // Flush requirement: within 2 cols of the right edge — except a
    // `/`-ending row, which relaxes to 8 cols.  claudecode's `⎿ `
    // indent blocks wrap paths at a fixed CONTENT width a few cells
    // short of the pane edge, so a wrapped path's first row never
    // passed the strict test and the path stayed split (2026-07-18
    // field report).  A prose row ending in `/` is rare enough that
    // the wider window stays conservative; File candidates keep the
    // stat + boundary-retry arbitration either way.
    // Does the row end in the middle of a *path*?  Walk back over the
    // trailing url/path-class run and look for a separator: a token
    // carrying `/` that runs to the end of the row is a path the
    // renderer cut, not a word that happened to finish there.
    //
    // This is the same observation the `/`-ending case above was
    // built on, generalised.  That one only caught a break landing
    // exactly on a separator; a break one character later — inside
    // `…-e18-ve` / `rdict.md` — fell back to 2 cells of slack, missed
    // by one, and the link stopped at the last directory that
    // happened to exist on disk (2026-08-16 field report).  Which
    // character the wrap lands on is not something the path controls.
    let trailing_is_path = {
        let mut c = last_nb_col;
        let mut has_sep = false;
        loop {
            let ch = src.char_at(c, prev_row);
            if !is_url_path_class(ch) {
                break;
            }
            if ch == '/' {
                has_sep = true;
                break;
            }
            if c == left {
                break;
            }
            c -= 1;
        }
        has_sep
    };
    // How far short of the edge the previous row may stop.
    //
    // claudecode does not wrap at the pane edge: it wraps at its own
    // CONTENT width, which sits an indent inside and differs per block
    // (`⏺`, `⎿ `, plain prose).  A fixed 8 was measured off one block
    // and lands exactly on the boundary for another — a 73-col pane
    // wrapping a path at column 65 missed by ONE cell, and the link
    // stopped at `…/lab36-continus/` (2026-09-06 field report).
    //
    // Stop guessing at the geometry.  A trailing token that carries a
    // `/` is a strong signal on its own, and a File match still has to
    // survive `stat`, with the seam offered as a candidate end if the
    // join was wrong.  Prose keeps the tight bound.
    let flush_slack: u16 = if last_nb_ch == '/' || trailing_is_path {
        cols / 3
    } else {
        2
    };
    if last_nb_col < cols.saturating_sub(flush_slack) {
        return false;
    }
    if !is_url_path_class(last_nb_ch) {
        return false;
    }
    // Current row leading whitespace, followed by a URL/path-class
    // char.  Zero indent is the weakest signal (flush prose looks the
    // same) — only accept it when the prev row is COMPLETELY full, as
    // a mid-word char wrap must be.
    let mut lead = left;
    while lead < right {
        let ch = src.char_at(lead, curr_row);
        if ch == ' ' || ch == '\0' {
            lead += 1;
        } else {
            break;
        }
    }
    // How far the continuation may hang.
    //
    // 4 was measured off claudecode's prose blocks.  Its `⎿` result
    // blocks hang at FIVE (`  ⎿  ` is two spaces, the glyph, two
    // spaces), and every path they wrapped was missed by exactly one
    // cell — the link stopped at the last directory that happened to
    // exist, `…/lab36-continus/` again (2026-09-08 field report, the
    // third time that same directory has been the visible symptom).
    //
    // Counted off five live sessions' bytelogs, over every wrap whose
    // previous row ends inside a path token (`\r ESC[nC ESC[1B`,
    // n = 17,991): indent 0 → 43.9 %, 2 → 20.1 %, **5 → 27.8 %**,
    // 7 → 2.5 %, 4 → 2.1 %, 6 → 1.4 %, 8 → 0.2 %.  Through 8 that is
    // 98.2 % of them; what is left starts at 9 and is dominated by 37,
    // which is column alignment inside a two-column block, not a
    // hanging wrap.
    //
    // Widened only for a path-carrying tail — the same trade
    // `flush_slack` above already makes.  A wrong join costs a File
    // match nothing: the merged span fails `stat` and the seam is
    // offered as a candidate end, while `cc_zero_indent` keeps URL and
    // Email matches from crossing the join at all.  Prose keeps 4,
    // where a wrong join has no `stat` to arbitrate it.
    let max_indent: u16 = if trailing_is_path { 8 } else { 4 };
    if lead - left > max_indent {
        return false;
    }
    // Zero indent is the weakest signal — flush prose looks identical —
    // so it used to demand the previous row be COMPLETELY full, as a
    // mid-word character wrap would be.  claudecode's zero-indent
    // continuations are mid-word wraps at its content width, not the
    // pane edge, so that demand never held for them.  Require the same
    // path-carrying tail instead: `cc_zero_indent` already forbids URL
    // and Email matches from crossing this join, leaving File matches,
    // which `stat` arbitrates.
    if lead == left && last_nb_col != right - 1 && !trailing_is_path {
        return false;
    }
    if lead >= right {
        return false;
    }
    // A wrap cuts a token in half, so the row below opens with the
    // REST of that token.  A lone `-` / `*` / `1.` followed by a
    // blank is not a token tail — it is a list marker the renderer
    // put there.  Geometry cannot see the difference (a nested list
    // item carries the same 1..=4 cell indent a hanging wrap does,
    // and the item above can end flush at the edge on a path char),
    // and joining a marker can only ever bolt one punctuation char
    // onto the row above: the 2026-08-18 field report was a bullet's
    // URL coming out as `…/village/index-`, having swallowed the `-`
    // that opened the NEXT bullet.
    if starts_list_marker(src, curr_row, lead, right) {
        return false;
    }
    let first = src.char_at(lead, curr_row);
    is_url_path_class(first)
}

/// Does `row` open a list item at `from` — a bullet (`-`, `*`, `+`,
/// `>`, …) or an ordered marker (`1.`, `2)`) standing alone before a
/// blank?  Anything longer than a marker, or not followed by a
/// blank, is ordinary text: a wrapped token resumes as one run of
/// characters, it does not spell a one-character word.
fn starts_list_marker<S: CellSource>(src: &S, row: u16, from: u16, right: u16) -> bool {
    let mut tok = ['\0'; 6];
    let mut n = 0usize;
    let mut c = from;
    while c < right && n < tok.len() {
        let ch = src.char_at(c, row);
        if ch == ' ' || ch == '\0' {
            break;
        }
        tok[n] = ch;
        n += 1;
        c += 1;
    }
    // Ran to the buffer's end without hitting a blank — too long to
    // be a marker, and the cell after `c` was never examined.
    if n == 0 || n == tok.len() {
        return false;
    }
    if n == 1 {
        return !tok[0].is_alphanumeric();
    }
    tok[..n - 1].iter().all(|c| c.is_ascii_digit()) && matches!(tok[n - 1], '.' | ')')
}

/// Char class that we consider "could be part of a URL or path
/// continuation".  Used by the cc hard-wrap heuristic to gate the
/// merge; the actual pattern scan still validates structure.
pub(crate) fn is_url_path_class(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '.' | '-' | '_' | '~' | '?' | '&' | '=' | '#' | '%' | ':' | '+' | '@' | ','
        )
}

pub(crate) fn count_leading_ws<S: CellSource>(src: &S, row: u16, cols: u16) -> u16 {
    let mut n = 0u16;
    while n < cols {
        let ch = src.char_at(n, row);
        if ch == ' ' || ch == '\0' {
            n += 1;
        } else {
            break;
        }
    }
    n
}
