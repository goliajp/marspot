//! The table-cell pass: links a table wrapped across several rows of one cell.

use crate::line::{LineSegment, scan_logical_line};
use crate::oracle::PathOracle;
use crate::sink::{LineBufs, Sink, TableScratch, TextBufs};
use crate::wrap::is_hard_wrap_continuation_in;
use crate::{CellSource, LinkRange};

/// The horizontal rules a table draws between its rows.  A row made
/// only of these (plus the junctions and blanks) separates two table
/// rows, so text above it and text below it belong to different cells
/// however aligned they look.
fn is_horizontal_rule(c: char) -> bool {
    matches!(c, '\u{2500}' | '\u{2501}' | '\u{2550}' | '\u{253C}' | '\u{254B}'
                | '\u{252C}' | '\u{2534}' | '\u{251C}' | '\u{2524}' | '\u{256A}'
                | '\u{256C}' | '\u{2566}' | '\u{2569}' | '\u{2560}' | '\u{2563}'
                | '\u{250C}' | '\u{2510}' | '\u{2514}' | '\u{2518}'
                | '\u{256D}' | '\u{256E}' | '\u{256F}' | '\u{2570}' | '-' | '=')
        || is_vertical_rule(c)
}

/// Is this row a separator between two table rows?
fn is_rule_row<S: CellSource>(src: &S, row: u16, left: u16, right: u16) -> bool {
    let mut saw_rule = false;
    for c in left..right {
        let ch = src.char_at(c, row);
        if ch == ' ' || ch == '\0' {
            continue;
        }
        if !is_horizontal_rule(ch) {
            return false;
        }
        saw_rule = true;
    }
    saw_rule
}

/// Does this row's cell start a fresh link rather than continue one?
///
/// The geometry test cannot tell "the cell wrapped mid-token" from
/// "the next table row's cell happens to be flush too" — a column
/// sized by its longest URL makes every row look full.  A scheme
/// (`https://`, `file://`, …) at the start of the continuation is the
/// giveaway: wrapped text resumes mid-token, it does not begin a new
/// address.
fn starts_new_scheme<S: CellSource>(src: &S, row: u16, from: u16, right: u16) -> bool {
    let mut seen = 0usize;
    let mut buf = ['\0'; 10];
    let mut c = from;
    while c < right && seen < buf.len() {
        let ch = src.char_at(c, row);
        if ch == '\0' || (ch == ' ' && seen == 0) {
            c += 1;
            continue;
        }
        buf[seen] = ch;
        seen += 1;
        c += 1;
    }
    let head = &buf[..seen];
    let Some(pos) = head.windows(3).position(|w| w == [':', '/', '/']) else { return false };
    pos > 0 && head[..pos].iter().all(|&c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
}

/// Vertical rules a table draws between its cells.  Box-drawing light
/// / heavy / double, plus the ASCII pipe that markdown renderers and
/// `column -t` emit.
pub(crate) fn is_vertical_rule(c: char) -> bool {
    matches!(c, '\u{2502}' | '\u{2503}' | '\u{2506}' | '\u{2507}'
                | '\u{250A}' | '\u{250B}' | '\u{2551}' | '|')
}

/// Table-cell pass: a URL (or path) that a table wrapped across
/// several rows of ONE cell.
///
/// The row-level merge cannot see this.  It asks whether the previous
/// row ran out of room at the PANE's right edge, and a table cell runs
/// out of room at its own border — several columns short, with the
/// border glyph itself sitting where the heuristic looks for the last
/// character of the token.  So a URL in a table stayed cut at the cell
/// boundary, and the visible link was whatever prefix fit in the first
/// row (2026-07-28 field report).
///
/// What this does instead: find blocks of consecutive rows that share
/// at least two vertical rules (that is what makes them a table), and
/// scan each column band between two rules as its own logical line,
/// joined across rows by the same continuation test measured against
/// the BAND's edges.
///
/// Only multi-row matches are emitted.  A band that produced a single
/// row's worth of match adds nothing the row pass did not already
/// find, and emitting it again would be a duplicate the hit-test has
/// to arbitrate.  Whatever the row pass found on those same cells —
/// necessarily the truncated prefix — is dropped by
/// `drop_shadowed_ranges` in favour of the whole thing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_table_cells<S: CellSource>(
    src: &S,
    cols: u16,
    rule_cols: &[(u16, u16)],
    out: &mut Sink,
    oracle: &dyn PathOracle,
    cwd: Option<&str>,
    table: &mut TableScratch,
    bufs: &mut TextBufs,
) {
    if rule_cols.len() < 4 {
        return;
    }
    // rule_cols is (row, col), pushed in row-major order by the scan.
    let mut row_start = 0usize;
    let TableScratch { block_rows, shared, next: scratch, cell } = table;
    block_rows.clear();
    shared.clear();
    scratch.clear();
    while row_start < rule_cols.len() {
        let row = rule_cols[row_start].0;
        let mut row_end = row_start;
        while row_end < rule_cols.len() && rule_cols[row_end].0 == row {
            row_end += 1;
        }
        let this_row: &[(u16, u16)] = &rule_cols[row_start..row_end];
        let contiguous = block_rows.last().is_some_and(|r| r + 1 == row);
        if contiguous {
            scratch.clear();
            scratch.extend(
                shared
                    .iter()
                    .copied()
                    .filter(|c| this_row.iter().any(|(_, rc)| rc == c)),
            );
            if scratch.len() >= 2 {
                std::mem::swap(shared, scratch);
                block_rows.push(row);
                row_start = row_end;
                continue;
            }
        }
        // This row does not extend the block — close what we have.
        if block_rows.len() >= 2 && shared.len() >= 2 {
            scan_table_block(src, block_rows, shared, cols, out, oracle, cwd, cell, bufs);
        }
        block_rows.clear();
        shared.clear();
        block_rows.push(row);
        shared.extend(this_row.iter().map(|(_, c)| *c));
        row_start = row_end;
    }
    if block_rows.len() >= 2 && shared.len() >= 2 {
        scan_table_block(src, block_rows, shared, cols, out, oracle, cwd, cell, bufs);
    }
}

/// One table block: scan every column band between adjacent rules.
#[allow(clippy::too_many_arguments)]
fn scan_table_block<S: CellSource>(
    src: &S,
    block_rows: &[u16],
    rules: &[u16],
    cols: u16,
    out: &mut Sink,
    oracle: &dyn PathOracle,
    cwd: Option<&str>,
    cell: &mut LineBufs,
    bufs: &mut TextBufs,
) {
    let LineBufs { chars, col_map, segments } = cell;
    for w in rules.windows(2) {
        let (left, right) = (w[0] + 1, w[1]);
        if right <= left + 1 {
            continue;
        }
        chars.clear();
        col_map.clear();
        segments.clear();
        let mut prev_row: Option<u16> = None;
        for &r in block_rows {
            if is_rule_row(src, r, left, right) {
                // A separator between table rows: whatever follows is
                // a different cell, so nothing crosses it.
                flush_table_line(chars, col_map, segments, cols, out, oracle, cwd, bufs);
                chars.clear();
                col_map.clear();
                segments.clear();
                prev_row = None;
                continue;
            }
            let joins = prev_row.is_some_and(|p| {
                is_hard_wrap_continuation_in(src, p, r, left, right)
                    && !starts_new_scheme(src, r, left, right)
            });
            if !joins {
                flush_table_line(chars, col_map, segments, cols, out, oracle, cwd, bufs);
                chars.clear();
                col_map.clear();
                segments.clear();
            } else {
                // The cell pads its content out to the border; those
                // blanks are layout, not text, and would terminate the
                // token at the very seam we are crossing.
                while chars.last() == Some(&' ') {
                    chars.pop();
                    col_map.pop();
                }
            }
            let mut col_skip = left;
            if joins {
                while col_skip < right {
                    let ch = src.char_at(col_skip, r);
                    if ch == ' ' || ch == '\0' {
                        col_skip += 1;
                    } else {
                        break;
                    }
                }
            }
            segments.push(LineSegment {
                phys_row: r,
                char_offset: chars.len(),
                cc_zero_indent: false,
                cc_seam: false,
            });
            let mut prev_was_wide = false;
            for c in col_skip..right {
                let ch = src.char_at(c, r);
                if prev_was_wide && ch == '\0' {
                    prev_was_wide = false;
                    continue;
                }
                chars.push(if ch == '\0' || ch == ' ' { ' ' } else { ch });
                col_map.push(c);
                prev_was_wide = ch != '\0' && src.is_wide(ch);
            }
            prev_row = Some(r);
        }
        flush_table_line(chars, col_map, segments, cols, out, oracle, cwd, bufs);
    }
}

/// Scan a band's logical line, but keep only matches that actually
/// crossed a row boundary — see `scan_table_cells`.
#[allow(clippy::too_many_arguments)]
fn flush_table_line(
    chars: &[char],
    col_map: &[u16],
    segments: &[LineSegment],
    cols: u16,
    out: &mut Sink,
    oracle: &dyn PathOracle,
    cwd: Option<&str>,
    bufs: &mut TextBufs,
) {
    if segments.len() < 2 {
        return;
    }
    let before = out.len;
    scan_logical_line(chars, col_map, segments, cols as usize, out, oracle, cwd, bufs);
    // A match confined to one row is one the row pass already had.
    let mut i = before;
    while i < out.len {
        let live = out.live();
        let same_text_rows = live[before..]
            .iter()
            .filter(|l| l.text == live[i].text)
            .count();
        if same_text_rows < 2 {
            out.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Two ranges on one row that cover overlapping cells are the same
/// link seen twice — the row pass's truncated prefix and the table
/// pass's whole token.  Keep the longer text; it is the one the click
/// should open.
pub(crate) fn drop_shadowed_ranges(out: &mut Sink) {
    // In place, no second Vec: this runs inside the per-frame scan,
    // which allocates nothing once warm.  Sorted by (row, col_start,
    // longest first), a range is shadowed exactly when it starts at or
    // before the furthest column any kept range on that row already
    // reaches.
    //
    // Insertion sort rather than `sort_by`: the std stable sort takes
    // a heap buffer past a few dozen elements, and every stable sort
    // gives the same order.  The input is mostly sorted already — the
    // row pass emits in row order and only the table pass's ranges
    // are out of place.
    let key = |l: &LinkRange| (l.row, l.col_start, std::cmp::Reverse(l.text.len()));
    let live = out.live_mut();
    for i in 1..live.len() {
        let k = key(&live[i]);
        let at = live[..i].partition_point(|l| key(l) <= k);
        live[at..=i].rotate_right(1);
    }
    let mut row = u16::MAX;
    let mut reach = 0u16;
    let mut kept = 0usize;
    for i in 0..live.len() {
        let l = &live[i];
        let keep = if l.row != row {
            row = l.row;
            reach = l.col_end;
            true
        } else if l.col_start <= reach {
            false
        } else {
            reach = reach.max(l.col_end);
            true
        };
        if keep {
            live.swap(kept, i);
            kept += 1;
        }
    }
    // the dropped ranges sit past `kept`, kept for their strings
    out.len = kept;
}

#[cfg(test)]
mod tests {
    use crate::tests::StrSource;
    use crate::{LinkKind, ScanOpts, scan_visible_links};

    /// 2026-07-28 field report: a URL inside a markdown table came
    /// out linked only as far as the first row of its cell.  The row
    /// merge asks whether the previous row ran out of room at the
    /// PANE edge; a table cell runs out at its own border, several
    /// columns short, with the border glyph itself sitting where the
    /// heuristic looks for the last character of the token.
    #[test]
    fn a_url_wrapped_across_one_table_cell_is_one_link() {
        // Two columns; the URL fills the right cell over three rows.
        // ASCII only: `StrSource` stores one char per column, so a
        // wide glyph here would misalign the rules against the rows
        // below it — a fixture artefact, not something a real grid
        // does (there the trail half occupies its own column).
        let src = StrSource::new(
            &[
                "│ file      │ Source link                     │",
                "│ detect.   │ https://raw.githubusercontent.c │",
                "│ caffemo   │ om/WeChatCV/opencv_3rdparty/a8b │",
                "│ del       │ 69ccc/detect.caffemodel         │",
                "├───────────┼─────────────────────────────────┤",
                "│ detect.p  │ same     /detect.prototxt       │",
            ],
            47,
        );
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        let full = "https://raw.githubusercontent.com/WeChatCV/opencv_3rdparty/a8b69ccc/detect.caffemodel";
        let url_rows: Vec<u16> = links
            .iter()
            .filter(|l| l.kind == LinkKind::Url && l.text == full)
            .map(|l| l.row)
            .collect();
        assert_eq!(
            url_rows,
            vec![1, 2, 3],
            "the URL should be one link underlined on all three of its rows, got {links:?}"
        );
        // …and the truncated prefix the row pass finds on row 1 must
        // not survive alongside it, or the click opens the wrong URL.
        let on_row_1: Vec<&str> = links
            .iter()
            .filter(|l| l.row == 1)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(on_row_1, vec![full], "row 1 must carry ONE link");

        // The underline stays inside the cell.  A multi-row span used
        // to run every row but its last out to the pane's right edge
        // — fine for a soft wrap, but here it drew straight through
        // the cell border and out the other side of the table.
        // Layout: `│ ` + 9-wide cell + ` │ ` + 31-wide cell + ` │`,
        // so the right cell's text lives in columns 14..=44 and its
        // border sits at 46.
        for l in links.iter().filter(|l| l.text == full) {
            assert!(
                l.col_start >= 14 && l.col_end <= 44,
                "row {} underlines {}..={}, outside the cell's 14..=44: {l:?}",
                l.row,
                l.col_start,
                l.col_end
            );
        }
    }

    /// The join is per cell, not per row.  Two table rows whose cells
    /// are both flush look exactly like one wrapped cell — which is
    /// what a column sized by its longest URL always looks like — so
    /// the fresh scheme on the second row is what has to stop it.
    #[test]
    fn table_cells_do_not_bleed_into_each_other() {
        let src = StrSource::new(
            &[
                "│ https://a.example/one │ plain words   │",
                "│ https://b.example/two │ more words    │",
            ],
            40,
        );
        let links = scan_visible_links(&src, ScanOpts { tui_mode: true, ..Default::default() });
        let mut texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
        texts.sort_unstable();
        texts.dedup();
        assert_eq!(
            texts,
            vec!["https://a.example/one", "https://b.example/two"],
            "each cell keeps its own link, got {links:?}"
        );
    }
}
