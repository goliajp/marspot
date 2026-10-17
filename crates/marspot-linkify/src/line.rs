//! The logical-line pattern scan, and projecting each match back onto the rows it covers.

use crate::LinkKind;
use crate::addr::{try_scan_ipv4_url, try_scan_ipv6, try_scan_uuid};
use crate::email::{is_email_host, scan_back_local, scan_forward_host};
use crate::oracle::PathOracle;
use crate::path::{
    is_left_boundary_pathish, join_cwd_into, looks_like_path, looks_like_relative_path,
    resolve_path_end, resolve_path_end_from, scan_path_candidate,
};
use crate::seam::{
    first_zero_indent_boundary, retry_file_at_segment_boundaries, url_stop_boundary,
};
use crate::sink::{Sink, TextBufs, collect_into, unquote_span};
use crate::url::{
    is_left_boundary_alnum, looks_like_url, matches_prefix, scan_until_link_terminator,
};

/// One physical row's contribution to a logical (post-soft-wrap-merge)
/// line.  `phys_row` is the viewport row the chars came from;
/// `char_offset` is where in the merged `chars` buffer this row's
/// pushed chars start.  May be smaller than `cols` when the row
/// contains wide-char trail halves (skipped) or has a stripped
/// cc-mode hanging indent.  Physical column for a given char_pos is
/// recovered via the parallel `col_map` slice, NOT linear arithmetic.
pub(crate) struct LineSegment {
    pub(crate) phys_row: u16,
    pub(crate) char_offset: usize,
    /// This segment was joined by the cc heuristic's WEAKEST form —
    /// zero-indent continuation (prev row completely full, this row
    /// starts at col 0).  That shape also matches ordinary flush
    /// prose, so matches without an existence oracle (URL, Email)
    /// are not allowed to cross this boundary; File matches may
    /// (stat + segment-boundary retry arbitrate).
    pub(crate) cc_zero_indent: bool,
    /// This segment was joined by the cc hard-wrap heuristic, so its
    /// `char_offset` is a SEAM: a point where claudecode broke one
    /// logical line in two.  When it broke at a space, that space is
    /// gone from both rows (the previous row's trailing blanks are
    /// popped, this row's hanging indent is skipped), so the merged
    /// text reads `…probe.sh-n 5` for what was `…probe.sh -n 5`.
    /// The seam is therefore a candidate end for a path, and a far
    /// better one than any punctuation guess — see `resolve_path_end`.
    pub(crate) cc_seam: bool,
}

/// Scan a logical (possibly multi-row-merged) line and emit
/// `LinkRange`s, one per **physical row** the match touches.  Single-
/// segment matches collapse to one LinkRange; multi-segment matches
/// fan out (same `text`, different `phys_row`/col_start/col_end),
/// preserving the per-row hit-test + per-row underline model.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_logical_line(
    chars: &[char],
    col_map: &[u16],
    segments: &[LineSegment],
    cols_per_row: usize,
    out: &mut Sink,
    oracle: &dyn PathOracle,
    cwd: Option<&str>,
    bufs: &mut TextBufs,
) {
    if segments.is_empty() {
        return;
    }
    // Pattern scan emits matches directly into `out` — the previous
    // intermediate `row_matches` Vec was a per-call allocation that
    // showed up as ~250 samples in the input-lag profile (15 s, 9
    // panes typing).  emit_match is the only producer, append-only,
    // so passing `out` straight through is safe and saves the alloc.
    scan_line_into_matches(chars, col_map, out, segments, cols_per_row, oracle, cwd, bufs);
}

/// Original pattern scanner, but the per-row emit step now consults
/// `segments` to fan a multi-row match out into one LinkRange per
/// physical row it covers.  Each per-row LinkRange carries the FULL
/// matched `text` (so click dispatch + hover tooltip are identical
/// across segments).
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_line_into_matches(
    chars: &[char],
    col_map: &[u16],
    out: &mut Sink,
    segments: &[LineSegment],
    cols_per_row: usize,
    oracle: &dyn PathOracle,
    cwd: Option<&str>,
    bufs: &mut TextBufs,
) {
    if segments.is_empty() {
        return;
    }
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // URL: http:// or https://
        if matches_prefix(chars, i, "http://") || matches_prefix(chars, i, "https://") {
            let mut end = scan_until_link_terminator(chars, i);
            // A zero-indent cc join is the weakest merge guess (flush
            // prose looks identical), and URLs have no existence
            // oracle to arbitrate — so a URL stops at one, unless the
            // break was forced (see `url_stop_boundary`).
            if let Some(b) = url_stop_boundary(chars, col_map, segments, cols_per_row, i, end) {
                end = b;
                while end > i
                    && matches!(
                        chars[end - 1],
                        ',' | '.' | ';' | ':' | ')' | ']' | '}' | '!' | '?'
                    )
                {
                    end -= 1;
                }
            }
            let span = &chars[i..end];
            if looks_like_url(span) {
                collect_into(&mut bufs.span, span);
                emit_match(out, segments, col_map, cols_per_row, i, end, LinkKind::Url, &bufs.span);
                i = end;
                continue;
            }
        }

        // A segment start is a text boundary even when the merged
        // buffer glues it to the previous row's last char — without
        // this, a path emitted via the boundary retry leaves the
        // NEXT row's own `/Users/...` with an alnum left neighbour
        // and it would be skipped as a mid-word slash.
        let at_seg_start = segments.iter().any(|s| s.char_offset == i);

        // `file://` — what claudecode emits when it points at a file
        // it just wrote.  Matched BEFORE the bare-path branch so the
        // scheme joins the link instead of being left dangling beside
        // it (the path after it matched on its own, so the link worked
        // but the selection started four characters late).
        //
        // Emitted as `File`, not `Url`: this names something on disk,
        // so it should face the same `stat` the bare path does — a URL
        // has no such arbiter and would underline a file that is not
        // there.  It also must NOT be a `Url`, because the click path
        // prefixes anything that is not `http(s)://` with `http://`,
        // which would open `http://file:///…`.
        if matches_prefix(chars, i, "file://") {
            let end = scan_path_candidate(chars, i);
            // Arbitrate on the PATH the scheme names, while the link's
            // span stays over what is drawn — the user selects and
            // sees `file://…`, and the click resolves the file.
            let path_start = i + "file://".len();
            // A `#fragment` / `?query` is URL syntax, not part of the
            // name on disk, so it is cut off before the filesystem is
            // asked and put back for the span: the browser is the one
            // that reads it, and `open` passes it through.
            //
            // Asking with it attached made the whole span fail, and the
            // arbitration then walked back to the next candidate that
            // does exist — across a wrapped line that is the directory
            // on the row above, so the link stopped at the seam and the
            // rest of the URL was left plain (2026-09-27 report:
            // `file:///…/goliajp/` ⏎ `vis/brand-dist/index-ja.html#principles`).
            let frag = (path_start..end).find(|&j| chars[j] == '#' || chars[j] == '?');
            let stat_end = frag.unwrap_or(end);
            if path_start < stat_end
                && let Some(b) = resolve_path_end(chars, path_start, stat_end, segments, oracle, bufs) {
                    // The fragment belongs to the link only when the
                    // whole path it hangs off resolved; a shorter
                    // arbitrated end means the rest is not this link's.
                    let span_end = if b == stat_end { end } else { b };
                    collect_into(&mut bufs.span, &chars[i..span_end]);
                    emit_match(
                        out, segments, col_map, cols_per_row, i, span_end, LinkKind::File,
                        &bufs.span,
                    );
                    i = span_end.max(i + 1);
                    continue;
                }
        }

        if c == '/' && i + 1 < n && (at_seg_start || !is_left_boundary_alnum(chars, i)) {
            let end = scan_path_candidate(chars, i);
            // Greedy scan, filesystem arbitration — `resolve_path_end`
            // is the whole decision.  The seam retry stays separate:
            // its candidates are cc hard-wrap boundaries, not prose
            // marks, and a seam prefix that happens to be a real
            // *directory* must not win over the file the line points
            // at, so it is tried only after every prose cut has.
            if let Some(b) = resolve_path_end(chars, i, end, segments, oracle, bufs) {
                unquote_span(bufs, &chars[i..b]);
                emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, &bufs.path);
                i = b.max(i + 1);
                continue;
            }
            // No `looks_like_path` guard on the whole span here.  The
            // span NOT looking like a path is precisely the case this
            // retry exists for: rows merged at a wrap seam produce
            // things like `…/join-bugs.sql/Users/…/describe/` — one
            // real path with another glued to its tail — and asking
            // whether the concatenation looks like a path answers no,
            // which is right and which is why the answer must not
            // gate the retry.  `retry_file_at_segment_boundaries`
            // applies `looks_like_path` to each candidate PREFIX,
            // where the question is the one worth asking.
            //
            // 2026-08-19 field report: three consecutive absolute
            // paths, each ending within the 8-column slack of the
            // right edge, merged into one logical line; the retry had
            // the correct answer at every step and never got asked.
            {
                if let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end, oracle, bufs) {
                    unquote_span(bufs, &chars[i..b]);
                    emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, &bufs.path);
                    i = b;
                    continue;
                }
            }
        }

        if c == '~'
            && i + 1 < n
            && chars[i + 1] == '/'
            && (at_seg_start || !is_left_boundary_alnum(chars, i))
        {
            let end = scan_path_candidate(chars, i);
            if end - i >= 3 {
                if let Some(b) = resolve_path_end(chars, i, end, segments, oracle, bufs) {
                    unquote_span(bufs, &chars[i..b]);
                    emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, &bufs.path);
                    i = b.max(i + 1);
                    continue;
                }
                if looks_like_path(&chars[i..end])
                    && let Some(b) = retry_file_at_segment_boundaries(chars, segments, i, end, oracle, bufs) {
                        unquote_span(bufs, &chars[i..b]);
                        emit_match(out, segments, col_map, cols_per_row, i, b, LinkKind::File, &bufs.path);
                        i = b;
                        continue;
                    }
            }
        }

        // Bare IPv4 (optionally :port and /path).  Emitted as `Ip` so
        // the click dispatcher's OpenLink arm prepends `http://` and
        // hands off to `/usr/bin/open`; Copy keeps the raw displayed
        // text.  Reject rules kill version strings (`v1.2.3.4`,
        // `1.2.3.4.5`, `1.2.3.4-rc1`), IPs inside longer identifiers,
        // and IPs already inside an `http://…` URL (prev char is `/`).
        if c.is_ascii_digit()
            && let Some(end) = try_scan_ipv4_url(chars, i) {
                collect_into(&mut bufs.span, &chars[i..end]);
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, &bufs.span,
                );
                i = end;
                continue;
            }

        // IPv6 — bracketed form (`[::1]:8080/foo`) triggers on `[`;
        // bare form (`2001:db8::1`, `::1`, or full 8-group) triggers
        // on hex or `:`.  `::` OR exactly 8 groups is required for
        // bare form so time strings (`12:34:56`) don't match.
        if c == '['
            && let Some(end) = try_scan_ipv6(chars, i, &mut bufs.span) {
                collect_into(&mut bufs.span, &chars[i..end]);
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, &bufs.span,
                );
                i = end;
                continue;
            }
        if (c.is_ascii_hexdigit() || c == ':')
            && let Some(end) = try_scan_ipv6(chars, i, &mut bufs.span) {
                collect_into(&mut bufs.span, &chars[i..end]);
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Ip, &bufs.span,
                );
                i = end;
                continue;
            }

        // UUID — canonical `8-4-4-4-12` hex.  Triggers on hex digit;
        // the shape is rigid enough that false-positive risk is
        // negligible without further gating.
        if c.is_ascii_hexdigit()
            && let Some(end) = try_scan_uuid(chars, i) {
                collect_into(&mut bufs.span, &chars[i..end]);
                emit_match(
                    out, segments, col_map, cols_per_row, i, end,
                    LinkKind::Uuid, &bufs.span,
                );
                i = end;
                continue;
            }

        if c == '@' && i > 0 && i + 1 < n {
            let local_start = scan_back_local(chars, i);
            let host_end = scan_forward_host(chars, i + 1);
            if local_start < i
                && host_end > i + 1
                && is_email_host(&chars[i + 1..host_end])
                // Same weak-guess rule as URLs: an address glued out
                // of two flush prose rows is not an address.
                && first_zero_indent_boundary(segments, local_start, host_end).is_none()
            {
                collect_into(&mut bufs.span, &chars[local_start..host_end]);
                emit_match(
                    out,
                    segments,
                    col_map,
                    cols_per_row,
                    local_start,
                    host_end,
                    LinkKind::Email,
                    &bufs.span,
                );
                i = host_end;
                continue;
            }
        }

        // Relative paths, LAST — every other kind gets first refusal,
        // so a URL, an address, an IP or a UUID is never re-read as a
        // filename.  Only reachable when the pane's directory is
        // known: `src/main.rs` names nothing on its own.
        if let Some(cwd) = cwd
            && (c.is_alphanumeric() || c == '.' || c == '_')
                && (at_seg_start || !is_left_boundary_pathish(chars, i))
            {
                let end = scan_path_candidate(chars, i);
                if end > i && looks_like_relative_path(&chars[i..end])
                    && let Some(b) =
                        resolve_path_end_from(chars, i, end, segments, oracle, Some(cwd), bufs)
                    {
                        unquote_span(bufs, &chars[i..b]);
                        join_cwd_into(&mut bufs.joined, cwd, &bufs.path);
                        emit_match_to(
                            out, segments, col_map, cols_per_row, i, b,
                            LinkKind::File, &bufs.path, Some(&bufs.joined),
                        );
                        i = b.max(i + 1);
                        continue;
                    }
            }

        i += 1;
    }
}

/// Project one `[char_lo, char_hi)` match onto the physical rows it
/// touches and push one `LinkRange` per row.  Single-row matches turn
/// into one LinkRange (unchanged from the legacy per-row scanner);
/// matches spanning N rows turn into N LinkRanges with the same
/// `text` and matching per-row col spans.
#[allow(clippy::too_many_arguments)]
fn emit_match(
    out: &mut Sink,
    segments: &[LineSegment],
    col_map: &[u16],
    cols_per_row: usize,
    char_lo: usize,
    char_hi_exclusive: usize,
    kind: LinkKind,
    text: &str,
) {
    emit_match_to(out, segments, col_map, cols_per_row, char_lo, char_hi_exclusive, kind, text, None)
}

/// [`emit_match`] carrying an explicit target — what the span NAMES,
/// when that differs from what it shows.  See `LinkRange::target`.
#[allow(clippy::too_many_arguments)]
fn emit_match_to(
    out: &mut Sink,
    segments: &[LineSegment],
    col_map: &[u16],
    cols_per_row: usize,
    char_lo: usize,
    char_hi_exclusive: usize,
    kind: LinkKind,
    text: &str,
    target: Option<&str>,
) {
    if char_lo >= char_hi_exclusive || segments.is_empty() {
        return;
    }
    // One LinkRange per physical row the match touches, and each
    // row's span is the cells it ACTUALLY covers — read back from
    // `col_map`, the same projection `locate()` uses.
    //
    // The earlier shape ran every row but the last out to the pane's
    // right edge, on the reasoning that a wrapped line fills its row.
    // That holds for a DECAWM soft wrap and nothing else: a cc-mode
    // hard wrap stops a few columns short, and a table cell stops at
    // its border — where the underline then ran on through the border
    // and out the far side of the table (2026-07-28, visible the
    // moment table cells started merging).  Reading the columns back
    // is both simpler and right in all three cases; for the soft wrap
    // it produces the identical answer, because there the last char
    // really is in the last column.
    //
    // `cols_per_row` still bounds the projection: a column at or past
    // the row width is not a cell anyone can click.
    for (i, seg) in segments.iter().enumerate() {
        let seg_lo = seg.char_offset;
        let seg_hi = segments
            .get(i + 1)
            .map(|s| s.char_offset)
            .unwrap_or(col_map.len());
        let lo = seg_lo.max(char_lo);
        let hi = seg_hi.min(char_hi_exclusive);
        if lo >= hi {
            continue;
        }
        let (Some(&c0), Some(&c1)) = (col_map.get(lo), col_map.get(hi - 1)) else {
            continue;
        };
        if c0 as usize >= cols_per_row || c1 < c0 {
            continue;
        }
        let c1 = c1.min(cols_per_row.saturating_sub(1) as u16);
        // Every segment carries the FULL text, so click dispatch is
        // the same wherever the user hit it.
        out.push(seg.phys_row, c0, c1, kind, text, target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsOracle, LinkRange};

    /// The pattern scanner over a hand-built logical line.
    fn line_matches(
        chars: &[char],
        col_map: &[u16],
        out: &mut Vec<LinkRange>,
        segments: &[LineSegment],
        cols: usize,
    ) {
        let mut targets = Vec::new();
        let len = out.len();
        let mut sink = Sink { out, len, targets: &mut targets };
        let mut bufs = TextBufs::default();
        scan_line_into_matches(chars, col_map, &mut sink, segments, cols, &FsOracle, None, &mut bufs);
        let len = sink.len;
        out.truncate(len);
    }

    // Phase 1 — soft-wrap-aware visible-grid scanning.  A URL or
    // absolute path that overflowed the right edge into a DECAWM
    // continuation row is matched as a single logical token and
    // emitted as one LinkRange per physical row segment.
    #[test]
    fn url_spanning_soft_wrap_emits_per_row_segments() {
        // 10-col grid; URL is 13 chars so it spans row 0 (cols 0..=9)
        // + row 1 (cols 0..=2).  Test the scanner directly on a
        // pre-built logical line + segment table.
        let line = "https://x.com";
        let segments = vec![
            super::LineSegment {
                phys_row: 0,
                char_offset: 0,
                cc_zero_indent: false,
                cc_seam: false,
            },
            super::LineSegment {
                phys_row: 1,
                char_offset: 10,
                cc_zero_indent: false,
                cc_seam: false,
            },
        ];
        let mut out = Vec::new();
        // col_map: chars 0..9 → row-0 cols 0..9; chars 10..12 → row-1 cols 0..2.
        let col_map: Vec<u16> = (0..10).chain(0..3).collect();
        let chars: Vec<char> = line.chars().collect();
        line_matches(&chars, &col_map, &mut out, &segments, 10);
        // One match, fanned into 2 LinkRanges (one per physical row).
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://x.com");
        assert_eq!(out[0].row, 0);
        assert_eq!(out[0].col_start, 0);
        assert_eq!(out[0].col_end, 9);
        assert_eq!(out[1].kind, LinkKind::Url);
        assert_eq!(out[1].text, "https://x.com");
        assert_eq!(out[1].row, 1);
        assert_eq!(out[1].col_start, 0);
        assert_eq!(out[1].col_end, 2);
    }

    #[test]
    fn single_row_match_still_emits_one_segment() {
        // No wrap: 1 segment, 1 LinkRange (regression guard for the
        // common case after the multi-row refactor).
        let line = "see https://example.com today      ";
        let segments = vec![super::LineSegment {
            phys_row: 7,
            char_offset: 0,
            cc_zero_indent: false,
                cc_seam: false,
        }];
        let mut out = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let col_map: Vec<u16> = (0..chars.len() as u16).collect();
        line_matches(&chars, &col_map, &mut out, &segments, chars.len());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 7);
        assert_eq!(out[0].kind, LinkKind::Url);
        assert_eq!(out[0].text, "https://example.com");
    }
}
