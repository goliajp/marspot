//! Grid → marspot-linkify adapter (steel tier).
//!
//! The actual detection engine lives in the `marspot-linkify` stone
//! crate — URLs / paths / emails / IPs / UUIDs, DECAWM merge,
//! fixed-width hard-wrap merge, composer exemption.  This module
//! only teaches it to read marspot's `Grid` at a scrollback view
//! offset, and preserves the historical call surface
//! (`scan_visible_links(grid, view_offset, opts)` + `ScanOpts` with
//! the marspot-flavoured `cc_mode` name) so render / hit-test call
//! sites stay untouched.
//!
//! Grid- and parser-coupled regression tests stay here (they need a
//! real `Terminal` / `Grid`); the pure-text battle-history tests
//! moved into the stone next to the code they pin.

use crate::grid::Grid;
use marspot_linkify::CellSource;

/// Re-exported for whoever ACTS on a file link.  Detection and action
/// have to agree on what `~/…` means: linkify expands it to decide the
/// path exists, so the opener must expand it too or the link resolves
/// in one place and fails in the other.
pub use marspot_linkify::expand_user_path;
pub use marspot_linkify::{FsOracle, LinkKind, LinkRange, NoFsOracle, PathOracle, PathVerdict};

/// Options that tune `scan_visible_links` for the calling pane.  All
/// fields default to off so the existing call path stays opt-in.
#[derive(Default, Clone, Copy, Debug)]
pub struct ScanOpts {
    /// claudecode-shaped pane: enable the stone's `tui_mode`
    /// (fixed-width hard-wrap merge + input-box exemption).
    pub cc_mode: bool,
}

struct GridSource<'a> {
    grid: &'a Grid,
    view_offset: u16,
}

impl CellSource for GridSource<'_> {
    fn cols(&self) -> u16 {
        self.grid.cols()
    }
    fn rows(&self) -> u16 {
        self.grid.rows()
    }
    fn char_at(&self, col: u16, row: u16) -> char {
        self.grid.cell_at_view(self.view_offset, col, row).ch
    }
    fn is_soft_wrap_continuation(&self, row: u16) -> bool {
        row > 0 && self.grid.wrapped_at_view(self.view_offset, row)
    }
    fn cursor(&self) -> (u16, u16) {
        self.grid.cursor()
    }
    fn is_wide(&self, ch: char) -> bool {
        crate::grid::char_width(ch) == 2
    }
}

/// Walk the visible grid and return every detected span.  See the
/// stone crate's `scan_visible_links` for the full semantics.
pub fn scan_visible_links(grid: &Grid, view_offset: u16, opts: ScanOpts) -> Vec<LinkRange> {
    scan_visible_links_with(grid, view_offset, opts, &FsOracle)
}

/// [`scan_visible_links`] with an explicit path oracle.  The render
/// loop passes a non-blocking, cached one — see `marspot::link_probe`
/// — so `build_instances` issues no filesystem syscalls.
pub fn scan_visible_links_with(
    grid: &Grid,
    view_offset: u16,
    opts: ScanOpts,
    oracle: &dyn PathOracle,
) -> Vec<LinkRange> {
    marspot_linkify::scan_visible_links_with(
        &GridSource { grid, view_offset },
        marspot_linkify::ScanOpts {
            tui_mode: opts.cc_mode,
        },
        oracle,
    )
}

#[cfg(test)]
mod grid_tests {
    use super::*;
    use crate::grid::{Cell, Grid};

    // End-to-end test through the REAL VT parser: feed a long URL +
    // newline to a Terminal and check that DECAWM-wrap flag really
    // gets set on the continuation row AND scan_visible_links picks
    // it up as a multi-row LinkRange.  This is the production path —
    // if it passes but the user's `echo` still shows broken wrap, the
    // bug is in render-side coords or some upstream layer.
    /// 2026-09-04 field report, at the grid level: a path that
    /// DECAWM-wraps across the last two rows, with the caret left on
    /// the continuation.  The stone's unit test pins the decision;
    /// this pins that a real `Terminal` still reaches it.
    #[test]
    fn a_wrapped_path_at_the_caret_survives_cc_mode() {
        use crate::terminal::Terminal;
        let root =
            std::env::temp_dir().join(format!("marspot-gridlinks-caret-{}", std::process::id()));
        let deep = root.join(".claude").join("notes");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("probe.sh");
        std::fs::write(&file, b"x").unwrap();
        let full = file.display().to_string();

        // A width that splits the path mid-name, so the row on its
        // own is not a real path and a truncating scan has somewhere
        // shorter to fall back to.
        let cols = (full.chars().count() as u16) - 3;
        let rows = 6u16;
        let mut t = Terminal::new(cols, rows);
        // Push the path down so its wrapped tail lands on the last
        // row and the caret rests there — the composer branch only
        // looks at a caret near the bottom, which is where a caret
        // sits after ordinary output.
        for _ in 0..(rows - 2) {
            t.feed(b"\r\n");
        }
        t.feed(full.as_bytes());
        let links = scan_visible_links(t.grid(), 0, super::ScanOpts { cc_mode: true });
        let texts: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
        std::fs::remove_dir_all(&root).ok();

        assert!(
            texts.iter().any(|x| *x == full),
            "wrapped path truncated in cc_mode: {texts:?}",
        );
    }

    /// 2026-09-04 field report, both halves, at the grid level: the
    /// three shapes the paths actually appeared in, across the widths
    /// the pane could have been at.  Wrapped or not, indented or not,
    /// the caret resting on the last line must not cost the link.
    #[test]
    fn field_report_paths_survive_a_caret_at_the_bottom() {
        use crate::terminal::Terminal;
        let root =
            std::env::temp_dir().join(format!("marspot-gridlinks-shapes-{}", std::process::id()));
        let deep = root.join(".claude").join("notes");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("provenance-probe.sh");
        std::fs::write(&file, b"x").unwrap();
        let p = file.display().to_string();
        let n = p.chars().count() as u16;

        let mut misses: Vec<String> = Vec::new();
        // Widths that wrap it, split it mid-name, and leave it whole.
        for cols in [n - 12, n - 3, n, n + 6, n + 40] {
            for (label, line) in [
                ("bare", p.clone()),
                ("indented + trailing arg", format!("  {p} -n 5")),
            ] {
                let mut t = Terminal::new(cols, 8);
                for _ in 0..5 {
                    t.feed(b"\r\n");
                }
                t.feed(line.as_bytes());
                let links = scan_visible_links(t.grid(), 0, super::ScanOpts { cc_mode: true });
                if !links.iter().any(|l| l.text == p) {
                    let got: Vec<&str> = links.iter().map(|l| l.text.as_str()).collect();
                    misses.push(format!("cols={cols} {label}: {got:?}"));
                }
            }
        }
        std::fs::remove_dir_all(&root).ok();
        assert!(
            misses.is_empty(),
            "link lost or truncated:\n  {}",
            misses.join("\n  ")
        );
    }

    #[test]
    fn scan_visible_links_via_real_parser() {
        use crate::terminal::Terminal;
        const COLS: u16 = 30;
        const ROWS: u16 = 5;
        let mut t = Terminal::new(COLS, ROWS);
        // 35-char URL — overflows 30-col grid, DECAWM should wrap.
        let url = "https://example.com/abc/d.html";
        // Length is exactly 30 chars — fits in one row, won't trigger
        // wrap.  Bump the length: append a longer path.
        let url = format!("{url}/extra/segments/here");
        t.feed(url.as_bytes());
        // Confirm the parser wrote the URL onto row 0 + row 1 AND set
        // the wrap flag on row 1.
        let grid = t.grid();
        assert!(
            grid.row_wrapped(1),
            "row 1 should be flagged as DECAWM continuation"
        );
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        assert!(
            links.len() >= 2,
            "expected ≥2 LinkRanges (URL fanned across rows), got {}: {:?}",
            links.len(),
            links
        );
        // Both LinkRanges should carry the FULL URL.
        for l in &links {
            assert_eq!(l.kind, LinkKind::Url);
            assert_eq!(l.text, url, "all fanned LinkRanges share full URL text");
        }
        // First range on row 0, second on row 1.
        assert_eq!(links[0].row, 0);
        assert_eq!(links[1].row, 1);
    }

    // End-to-end test that mirrors how the production renderer drives
    // scan_visible_links: we build a real Grid, fill DECAWM-wrap-style
    // rows with a long URL filling the right edge + continuation on
    // the next row, set the wrap flag, then call scan_visible_links
    // and assert it emits TWO LinkRanges (top + continuation segment),
    // both carrying the full URL.
    #[test]
    fn scan_visible_links_handles_decawm_wrap() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 30;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // 35-char URL: spans cols 0..=29 (30 chars) on row 0 then
        // cols 0..=4 (5 chars) on row 1.  DECAWM would mark row 1
        // as the wrap continuation.
        let url = "https://example.com/path/long.htm";
        let url_chars: Vec<char> = url.chars().collect();
        assert_eq!(url_chars.len(), 33);
        // Row 0: cols 0..=29 = url chars 0..=29 (fills row entirely).
        for (c, &ch) in url_chars.iter().take(COLS as usize).enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // Row 1: cols 0..=2 = url chars 30..=32 (3 chars).  Remaining
        // cols stay default (blank).
        for (c, &ch) in url_chars.iter().skip(COLS as usize).enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // Mark row 1 as the wrap continuation of row 0.
        grid.set_row_wrapped(1, true);
        // Scan at view_offset=0 (live grid, top of viewport).
        let links = scan_visible_links(&grid, 0, super::ScanOpts::default());
        // Expect 2 LinkRanges: row 0 [0..=29] + row 1 [0..=2], both
        // carrying the full URL.
        assert_eq!(
            links.len(),
            2,
            "expected 2 LinkRanges (row 0 + row 1), got {}: {:?}",
            links.len(),
            links
        );
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].text, url);
        assert_eq!(links[0].row, 0);
        assert_eq!(links[0].col_start, 0);
        assert_eq!(links[0].col_end, 29);
        assert_eq!(links[1].kind, LinkKind::Url);
        assert_eq!(links[1].text, url);
        assert_eq!(links[1].row, 1);
        assert_eq!(links[1].col_start, 0);
        assert_eq!(links[1].col_end, 2);
    }

    /// End-to-end: feed real bytes through the VT parser and scan
    /// the resulting grid for links.  Mirrors the live L2 path
    /// (PTY → Terminal::feed → mirror grid → scan_visible_links →
    /// Cmd-click hit-test + renderer underline pass), so a fix that
    /// passes the unit test above but fails the parser pipeline gets
    /// caught here.
    /// cc-mode hard-wrap merge: build a grid where a URL is broken
    /// across two rows by a HARD newline (no DECAWM wrap flag set),
    /// with a 2-space hanging indent on the continuation row.  Without
    /// `cc_mode`, the scanner sees row 0's URL fragment + row 1's
    /// "/path..." separately and the regex won't match across.  With
    /// `cc_mode`, the line builder strips the indent and the URL
    /// regex picks up the whole token; the LinkRange fans out across
    /// both rows.
    #[test]
    fn cc_mode_merges_hard_wrap_url_across_indent() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // Row 0 (20 cols): "https://example.com/" — fills entire row,
        // last char at col 19 is '/'.
        let row0 = b"https://example.com/";
        for (c, &b) in row0.iter().enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch: b as char,
                    ..Default::default()
                },
            );
        }
        // Row 1: "  path/to/file.html" — 2-space hanging indent then
        // the URL continuation.  Note: NO wrap flag set.
        let row1 = b"  path/to/file.html";
        for (c, &b) in row1.iter().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch: b as char,
                    ..Default::default()
                },
            );
        }
        // Without cc_mode: row 0's URL terminates at the row edge; the
        // regex doesn't reach row 1.  scan_until_link_terminator only
        // sees row 0's chars + row 1's indent spaces → URL ends at
        // row 0.  But row 1 alone has no http:// so no second link.
        let no_cc = scan_visible_links(&grid, 0, ScanOpts::default());
        assert!(
            no_cc.iter().all(|l| l.row == 0),
            "no cc_mode: link should not span to row 1: {no_cc:?}"
        );

        // With cc_mode: heuristic kicks in (row 0 ends at col 19 with
        // '/', row 1 has 2 leading spaces then 'p' alphanum).  Indent
        // stripped → logical line is "https://example.com/path/to/file.html"
        // → URL regex matches whole token → LinkRange fans out.
        let opts = ScanOpts { cc_mode: true };
        let with_cc = scan_visible_links(&grid, 0, opts);
        assert!(
            with_cc.len() >= 2,
            "cc_mode: expected URL to span ≥2 rows after indent strip, got {with_cc:?}"
        );
        let expected = "https://example.com/path/to/file.html";
        for l in &with_cc {
            assert_eq!(l.kind, LinkKind::Url);
            assert_eq!(l.text, expected, "merged URL text mangled: {l:?}");
        }
        // Row 1's segment must start at col 2 (the indent was stripped
        // from the logical line, but locate() adds col_skip back so
        // the physical click target lines up with the visible chars).
        let row1_seg = with_cc.iter().find(|l| l.row == 1).expect("row 1 segment");
        assert_eq!(
            row1_seg.col_start, 2,
            "row 1 LinkRange must start at col 2 (after hanging indent)"
        );
    }

    /// cc-mode does NOT fire when the prev row's last char isn't
    /// URL/path-class — protects against accidentally merging two
    /// unrelated paragraphs.
    #[test]
    fn cc_mode_does_not_merge_when_prev_row_ends_with_punct() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut grid = Grid::new(COLS, ROWS);
        // Row 0 ends with a period (sentence end), not URL/path char.
        let row0 = b"finished the request.";
        for (c, &b) in row0.iter().take(COLS as usize).enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch: b as char,
                    ..Default::default()
                },
            );
        }
        // Row 1 has indent + URL-looking content.
        let row1 = b"  /some/path.rs";
        for (c, &b) in row1.iter().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch: b as char,
                    ..Default::default()
                },
            );
        }
        let with_cc = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        // Heuristic must reject the pair → no merge → row 0 + row 1
        // are scanned independently; path on row 1 has 2-space indent
        // before it but the path itself starts at col 2 — that's fine
        // (its own row's scan will pick it up if it stat()s; here we
        // just assert no merge happened by checking no LinkRange
        // carries text containing "request").
        for l in &with_cc {
            assert!(
                !l.text.contains("finished"),
                "cc_mode wrongly merged a sentence into the next row: {l:?}"
            );
        }
    }

    #[test]
    fn e2e_scan_links_via_parser_finds_url_across_soft_wrap() {
        use crate::terminal::Terminal;
        let cols = 20u16;
        let rows = 5u16;
        let mut t = Terminal::new(cols, rows);
        let url = "https://example.com/some/very/long/path/that/wraps?q=value";
        t.feed(url.as_bytes());
        let grid = t.grid();
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        assert!(!links.is_empty(), "no LinkRange emitted for {url:?}");
        for link in &links {
            assert_eq!(link.kind, LinkKind::Url);
            assert_eq!(
                link.text, url,
                "LinkRange text mangled by wrap merge: {:?}",
                link.text
            );
        }
        // The URL is longer than `cols`, so we should see >= 2 rows.
        assert!(
            links.len() >= 2,
            "expected multi-row LinkRange (cols=20, url len {}), got {} segments",
            url.chars().count(),
            links.len()
        );
    }

    /// Wide-char (CJK) trail-halves used to push as ' ' into the
    /// scan buffer, causing `scan_until_link_terminator` to cut a
    /// path on the first CJK boundary.  Regression guard: feed a
    /// path containing CJK chars through the real parser, stat
    /// it via a tempfile so `is_real_path` accepts it, and assert
    /// the FULL path is one contiguous LinkRange covering both the
    /// lead and trail cells of every wide char.
    #[test]
    fn cjk_path_detected_across_wide_char_cells() {
        use crate::terminal::Terminal;
        let dir = std::env::temp_dir().join("marspot-link-cjk-test");
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("決算明細_2025-2026.txt");
        std::fs::write(&path, b"x").expect("write tempfile");
        let path_str = path.to_string_lossy().into_owned();
        // Wide enough to keep the whole path on one row; the input
        // is ASCII `/private/...`-style, no soft-wrap concerns.
        let cols: u16 = (path_str.chars().count() as u16) + 10;
        let mut t = Terminal::new(cols, 3);
        t.feed(format!("see {} for", path_str).as_bytes());
        let grid = t.grid();
        let links = scan_visible_links(grid, 0, super::ScanOpts::default());
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            links.len(),
            1,
            "expected exactly one File LinkRange for the CJK path, got {links:?}"
        );
        let link = &links[0];
        assert_eq!(link.kind, LinkKind::File);
        assert_eq!(
            link.text, path_str,
            "LinkRange text truncated at a CJK boundary"
        );
        // col_start = col of '/'; col_end = col of last 't' in `.txt`.
        // The grid stores each wide char as lead+trail, so col_end -
        // col_start + 1 = total physical cells covered = path length
        // in chars + count of wide chars (each contributes one
        // extra cell vs `.chars().count()`).
        let wide_count = path_str
            .chars()
            .filter(|c| crate::grid::char_width(*c) == 2)
            .count() as u16;
        let expected_span = path_str.chars().count() as u16 + wide_count;
        assert_eq!(
            (link.col_end - link.col_start + 1),
            expected_span,
            "underline span ({}..={}) doesn't cover lead+trail of each wide char (expected {} cells)",
            link.col_start,
            link.col_end,
            expected_span
        );
    }

    /// 2026-07-12 second report: three shift+enter-separated real
    /// paths in claudecode's input box; #2 and #3 char-wrap mid-word
    /// at the right edge with ZERO hanging indent ("…roun" / "d-2.md")
    /// — the old 1..=4-indent requirement never merged them, so only
    /// path #1 got a link.  Zero-indent merge (gated on a completely
    /// full prev row) must recover all three; the flush row0/row1
    /// junction also exercises the glue-then-retry path (path #1 ends
    /// flush and path #2 starts at col 0 → merged, stat fails, retry
    /// splits at the boundary).
    #[test]
    fn zero_indent_char_wrap_paths_all_link() {
        use crate::grid::{Cell, Grid};
        let dir = std::env::temp_dir().join(format!("marspot-lnk0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p.to_string_lossy().into_owned()
        };
        let p1 = mk("smix-feedback-2026-07-12.md");
        let p2 = mk("smix-feedback-2026-07-12-round-2.md");
        let p3 = mk("qa-sim-behavior-verification-plan.md");
        let cols = p1.chars().count() as u16; // p1 exactly fills row 0

        let mut grid = Grid::new(cols, 8);
        let mut put = |row: u16, text: &str| {
            for (c, ch) in text.chars().enumerate() {
                grid.set_cell(
                    c as u16,
                    row,
                    Cell {
                        ch,
                        ..Default::default()
                    },
                );
            }
        };
        let (a2, b2) = p2.split_at(
            p2.char_indices()
                .nth(cols as usize)
                .map(|(i, _)| i)
                .unwrap(),
        );
        let (a3, b3) = p3.split_at(
            p3.char_indices()
                .nth(cols as usize)
                .map(|(i, _)| i)
                .unwrap(),
        );
        put(0, &p1);
        put(1, a2);
        put(2, b2);
        put(3, a3);
        put(4, b3);

        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        for f in [&p1, &p2, &p3] {
            let hits: Vec<_> = links
                .iter()
                .filter(|l| l.kind == LinkKind::File && l.text == **f)
                .collect();
            assert!(!hits.is_empty(), "path {f} must be detected; got {links:?}");
        }
        let texts: std::collections::HashSet<_> = links.iter().map(|l| l.text.clone()).collect();
        assert_eq!(texts.len(), 3, "exactly the three paths: {links:?}");
        for p in [p1, p2, p3] {
            let _ = std::fs::remove_file(dir.join(std::path::Path::new(&p).file_name().unwrap()));
        }
        let _ = std::fs::remove_dir(&dir);
    }

    /// 2026-07-13 report: a wrapped path immediately followed by
    /// `(Ask 12:…` prose lost its link.
    ///
    /// The first fix made `(` and the CJK fullwidth family hard
    /// terminators.  That over-corrected — a name containing one
    /// became unlinkable (2026-08-05) — so the scan is greedy again
    /// and the filesystem decides where the name ended.  Every case
    /// below still resolves to the same span, by asking rather than
    /// by guessing; `:note` still stays unlinked, which is the one
    /// shape this report settled as 宁可漏.
    #[test]
    fn path_terminates_at_paren_and_cjk_punct() {
        // Single-row cases through the scan_line path.
        let dir = std::env::temp_dir().join(format!("marspot-lnkp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plan.md");
        std::fs::write(&p, b"x").unwrap();
        let ps = p.to_string_lossy().into_owned();
        for suffix in ["(Ask 12", "、后续", "。end", "（全角）", ":120:5"] {
            // ":120:5" exercises the line-col suffix strip; the rest
            // exercise the new hard terminators.
            let line = format!("看 {}{} 即可", ps, suffix);
            let out = marspot_linkify::scan_text_line(&line, 0);
            let files: Vec<_> = out.iter().filter(|l| l.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "suffix {suffix:?}: {out:?}");
            assert_eq!(files[0].text, ps, "suffix {suffix:?}");
        }
        // `:`+prose glued onto a path is NOT a recognised shape —
        // stays unlinked rather than guessing (宁可漏).
        let out = marspot_linkify::scan_text_line(&format!("看 {}:note 即可", ps), 0);
        assert!(out.iter().all(|l| l.kind != LinkKind::File), "{out:?}");
        // Wrapped zero-indent + glued paren — the screenshot shape.
        use crate::grid::{Cell, Grid};
        let cols = ps.chars().count() as u16 - 10;
        let mut grid = Grid::new(cols, 4);
        let split_byte = ps
            .char_indices()
            .nth(cols as usize)
            .map(|(i, _)| i)
            .unwrap();
        let (a, b) = ps.split_at(split_byte);
        for (c, ch) in a.chars().enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        for (c, ch) in format!("{}(Ask 12", b).chars().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let files: Vec<_> = links.iter().filter(|l| l.kind == LinkKind::File).collect();
        assert!(
            files.iter().any(|l| l.text == ps),
            "wrapped path + glued paren must link: {links:?}"
        );
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// The zero-indent merge must NOT let a flush-ending URL absorb
    /// the next prose row (URLs have no existence oracle).
    #[test]
    fn zero_indent_does_not_glue_urls() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 30;
        let url = format!("https://example.com/{}", "a".repeat(10)); // 30 chars
        assert_eq!(url.chars().count(), COLS as usize);
        let mut grid = Grid::new(COLS, 4);
        for (c, ch) in url.chars().enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        for (c, ch) in "and more prose".chars().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].text, url, "URL must stop at the row edge");
        assert_eq!(links[0].row, 0);
    }

    /// 2026-07-12 regression: a real path that ends flush at the
    /// right edge, followed by a prose row starting with a
    /// path-class word ("fullpath 已交…"), tripped the cc hard-wrap
    /// heuristic — the merge produced `…feedback.mdfullpath`, the
    /// stat failed, and the perfectly valid single-row path lost its
    /// link.  The emit-time segment-boundary retry must recover it.
    #[test]
    fn flush_right_path_followed_by_prose_still_links() {
        // A REAL file; the grid is sized so the path exactly fills
        // row 0 (flush right = what trips the cc merge heuristic).
        let dir = std::env::temp_dir().join(format!("marspot-lnk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feedback.md");
        std::fs::write(&path, b"x").unwrap();
        let path_s = path.to_string_lossy().into_owned();
        let cols = path_s.chars().count() as u16;

        let mut grid = Grid::new(cols, 4);
        for (c, ch) in path_s.chars().enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // Continuation-looking prose row: 1-space indent + alnum word.
        for (c, ch) in " fullpath done".chars().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }

        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        assert_eq!(
            links.len(),
            1,
            "flush-right real path must survive the cc merge: {links:?}"
        );
        assert_eq!(links[0].kind, LinkKind::File);
        assert_eq!(links[0].text, path_s);
        assert_eq!(links[0].row, 0);
    }

    /// 2026-07-18 field report #2 — a path hard-wrapped inside a
    /// claudecode `⎿ ` indent block (first row ends in `/` a few
    /// cols short of the pane edge, continuation indented 3) must
    /// merge into ONE File link.  Pre-fix, the rows didn't merge
    /// (flush test wanted cols-2), the uuid path component on row 2
    /// got claimed as a standalone Uuid link, and the tail
    /// `/scratchpad/x.png` was rejected as a mid-word slash.
    #[test]
    fn cc_wrapped_path_with_uuid_component_links_whole() {
        use crate::grid::{Cell, Grid};
        let dir = std::env::temp_dir().join(format!("marspot-lnk-uuid-{}", std::process::id()));
        let uuid_dir = dir.join("3dbde79c-ab6e-43bd-8143-c448617e1d69/scratchpad");
        std::fs::create_dir_all(&uuid_dir).unwrap();
        let png = uuid_dir.join("env_now3_small.png");
        std::fs::write(&png, b"x").unwrap();
        let full = png.to_string_lossy().into_owned();
        // Split after the parent-of-uuid slash, like the field case.
        let split = full.find("3dbde79c").unwrap();
        let (row1_body, row2_body) = full.split_at(split);
        // Row 1 ends 4 cols short of the edge (claudecode ⎿ block).
        let cols = (row1_body.chars().count() + 4) as u16;
        let mut grid = Grid::new(cols, 6);
        let put = |grid: &mut Grid, row: u16, text: &str| {
            for (c, ch) in text.chars().enumerate() {
                if (c as u16) < cols {
                    grid.set_cell(
                        c as u16,
                        row,
                        Cell {
                            ch,
                            ..Default::default()
                        },
                    );
                }
            }
        };
        put(&mut grid, 0, row1_body);
        put(&mut grid, 1, &format!("   {row2_body}"));
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            links
                .iter()
                .any(|l| l.kind == LinkKind::File && l.text == full),
            "wrapped path must link whole: {links:?}"
        );
        assert!(
            links.iter().all(|l| l.kind != LinkKind::Uuid),
            "uuid path component must not become a Uuid link: {links:?}"
        );
    }

    /// 2026-07-18 field regression — claudecode v2.1.212 dropped the
    /// composer's rounded box, so the bottom-most `╭…╰` box on screen
    /// became the WELCOME BANNER; the cc-mode exemption swallowed it
    /// and the banner's email/path links vanished.  The exemption now
    /// requires the box to look like an active input area (contains
    /// the cursor row, or hugs the viewport bottom).
    #[test]
    fn cc_exemption_does_not_swallow_top_banner_without_composer_box() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 60;
        const ROWS: u16 = 12;
        let mut grid = Grid::new(COLS, ROWS);
        let put = |grid: &mut Grid, row: u16, text: &str| {
            for (c, ch) in text.chars().enumerate() {
                if (c as u16) < COLS {
                    grid.set_cell(
                        c as u16,
                        row,
                        Cell {
                            ch,
                            ..Default::default()
                        },
                    );
                }
            }
        };
        put(&mut grid, 0, " ╭──────────────────────────────╮  Tips for");
        put(&mut grid, 1, " │  Welcome back!               │  Run /init");
        put(
            &mut grid,
            2,
            " │  takagi@golia.jp's Org       │  What's new",
        );
        put(
            &mut grid,
            3,
            " │  ~/workspace                 │  Added fork",
        );
        put(&mut grid, 4, " ╰──────────────────────────────╯");
        put(
            &mut grid,
            7,
            " > bare composer, no box (claudecode v2.1.212)",
        );
        grid.set_cursor(3, 7);
        let with_cc = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        let texts: Vec<&str> = with_cc.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts.contains(&"takagi@golia.jp"),
            "banner email must survive cc_mode: {with_cc:?}"
        );
        assert!(
            texts.contains(&"~/workspace"),
            "banner path must survive cc_mode: {with_cc:?}"
        );

        // Counter-case: an actual bottom composer box (old claudecode
        // UI) still gets exempted — cursor inside it.
        let mut grid2 = Grid::new(COLS, ROWS);
        put(&mut grid2, 0, " see https://example.com/docs above");
        put(&mut grid2, 8, " ╭──────────────────────────────╮");
        put(&mut grid2, 9, " │ > typing https://foo.com/bar │");
        put(&mut grid2, 10, " ╰──────────────────────────────╯");
        grid2.set_cursor(30, 9);
        let links = scan_visible_links(&grid2, 0, ScanOpts { cc_mode: true });
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].text, "https://example.com/docs");
    }

    /// cc-mode input-box exemption: URL / IP / UUID inside the
    /// bottom-most `╭…╮` / `╰…╯` box (the claudecode composer) must
    /// NOT be detected — mid-typing text shouldn't flash underlined
    /// and clicks in the composer shouldn't hit link menus.  Content
    /// ABOVE the box (chat scrollback) still detects normally.
    #[test]
    fn cc_input_box_content_is_exempt_from_link_detection() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 40;
        const ROWS: u16 = 8;
        let mut grid = Grid::new(COLS, ROWS);
        // Row 0-1: chat scrollback with a URL — must detect.
        let history = "see https://example.com/page for docs";
        for (c, ch) in history.chars().enumerate() {
            grid.set_cell(
                c as u16,
                0,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // Row 3: box top border ╭─────╮
        grid.set_cell(
            0,
            3,
            Cell {
                ch: '╭',
                ..Default::default()
            },
        );
        for c in 1..(COLS - 1) {
            grid.set_cell(
                c,
                3,
                Cell {
                    ch: '─',
                    ..Default::default()
                },
            );
        }
        grid.set_cell(
            COLS - 1,
            3,
            Cell {
                ch: '╮',
                ..Default::default()
            },
        );
        // Row 4-5: box interior with a URL user is typing — must NOT detect.
        let typing = "│ > try https://foo.com/bar          │";
        for (c, ch) in typing.chars().enumerate() {
            grid.set_cell(
                c as u16,
                4,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        let typing2 = "│   47.96.114.231 also             │";
        for (c, ch) in typing2.chars().enumerate() {
            grid.set_cell(
                c as u16,
                5,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        // Row 6: box bottom border
        grid.set_cell(
            0,
            6,
            Cell {
                ch: '╰',
                ..Default::default()
            },
        );
        for c in 1..(COLS - 1) {
            grid.set_cell(
                c,
                6,
                Cell {
                    ch: '─',
                    ..Default::default()
                },
            );
        }
        grid.set_cell(
            COLS - 1,
            6,
            Cell {
                ch: '╯',
                ..Default::default()
            },
        );

        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        // Exactly one link, from row 0 (the scrollback URL).
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].kind, LinkKind::Url);
        assert_eq!(links[0].text, "https://example.com/page");
        assert_eq!(links[0].row, 0);
        // Sanity: without cc_mode, both the scrollback URL AND the
        // in-box URL / IP would surface — the exemption is what
        // suppresses the composer noise.
        let no_cc = scan_visible_links(&grid, 0, ScanOpts::default());
        assert!(
            no_cc.len() > 1,
            "without cc_mode the exemption must not fire: {no_cc:?}"
        );
    }

    /// A grid with `╰` at the very last row and no `╭` above (a
    /// pathological / truncated frame) must NOT trip the exemption —
    /// we'd rather scan a few false positives than silently swallow
    /// the whole grid.
    #[test]
    fn cc_input_box_requires_both_top_and_bottom() {
        use crate::grid::{Cell, Grid};
        const COLS: u16 = 30;
        const ROWS: u16 = 4;
        let mut grid = Grid::new(COLS, ROWS);
        // Only a bottom border, no top.
        grid.set_cell(
            0,
            ROWS - 1,
            Cell {
                ch: '╰',
                ..Default::default()
            },
        );
        grid.set_cell(
            COLS - 1,
            ROWS - 1,
            Cell {
                ch: '╯',
                ..Default::default()
            },
        );
        // A URL earlier in the grid.
        let url = "goto https://example.com/x";
        for (c, ch) in url.chars().enumerate() {
            grid.set_cell(
                c as u16,
                1,
                Cell {
                    ch,
                    ..Default::default()
                },
            );
        }
        let links = scan_visible_links(&grid, 0, ScanOpts { cc_mode: true });
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].text, "https://example.com/x");
    }
}

#[cfg(test)]
mod tilde_cjk_tests {
    use super::*;
    use crate::terminal::Terminal;

    /// The `~/` + CJK combination from the 2026-07-03 field report
    /// (`~/Downloads/GOLIA-代表取缔役印.png` not underlined): the `~`
    /// branch + wide-char trail-half handling + `is_real_path`'s
    /// HOME expansion must compose.  The sibling CJK test uses an
    /// absolute path, so it never exercises the tilde branch.
    ///
    /// `set_var("HOME")` is safe under nextest (process per test);
    /// under plain multi-threaded `cargo test` it could race other
    /// tests reading HOME — the project runner is nextest
    /// (bin/test.sh).
    #[test]
    fn tilde_cjk_path_detected() {
        let home = std::env::temp_dir().join("marspot-link-tilde-cjk");
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::fs::write(home.join("Downloads/GOLIA-代表取缔役印.png"), b"x").unwrap();
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("HOME", &home) };
        let s = "~/Downloads/GOLIA-代表取缔役印.png";
        let cols = (s.chars().count() as u16) * 2 + 20;
        let mut t = Terminal::new(cols, 3);
        t.feed(format!("see {s} ok").as_bytes());
        let links = scan_visible_links(t.grid(), 0, ScanOpts::default());
        assert_eq!(links.len(), 1, "got {links:?}");
        assert_eq!(links[0].text, s);
        assert_eq!(links[0].kind, LinkKind::File);
    }
}
