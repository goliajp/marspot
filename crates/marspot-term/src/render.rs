//! Render-layer data types + box-drawing helpers.
//!
//! This module used to host an AppKit/CGContext-based `Renderer`
//! alongside the Metal one in `render_metal.rs`. After mcli migrated
//! to Metal (everyone now goes through `MetalRenderer`), the AppKit
//! renderer was removed — what remains here is the bits both binaries
//! still share:
//!
//! - `SessionView`, `SidebarEntry` — the data types `MetalRenderer`
//!   consumes (the caller bundles a Grid + a few flags so the
//!   renderer doesn't need to know about Session / Marspot / Pane).
//! - `box_drawing_arms`, `block_element_rects` — char classifiers
//!   that identify the U+2500-U+257F + U+256D-U+2570 box-drawing
//!   chars and U+2580-U+259F block elements.
//! - `rasterize_arms_into_buf`, `rasterize_block_into_buf` — pure
//!   Rust per-cell mask rasterisers used by `render_metal` to fill
//!   the glyph atlas for those characters. The font's CT-rasterised
//!   glyphs for `─`/`│`/`╭` etc. don't span the cell advance, so
//!   blitting them as-is leaves visible gaps at borders; these
//!   masks fill the cell exactly so adjacent cells join with zero
//!   drift regardless of where the cell lands on the grid.

use crate::grid::Grid;
use crate::input_core::{MarspotKeyEvent, Modifiers};
use crate::session::SessionState;

/// C1 — where a `PaneTool` claims pane real estate.  TopFixed /
/// BottomFixed tools each subtract their `fixed_height_rows()` worth
/// of cell-rows from the grid's inner rect; Overlay tools float and
/// do not affect grid layout.  See `docs/scrollback-search.md` §6.1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolSlot {
    TopFixed,
    BottomFixed,
    Overlay,
}

/// C1 — what a tool's input handler returns.  `Pass` means "I didn't
/// handle this; let the next layer try."  `Handled` means stop;
/// `HandledRequestRedraw` is `Handled` + please redraw.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InputDisposition {
    Pass,
    Handled,
    HandledRequestRedraw,
}

/// C4 — viewport-row-keyed highlight span.  Renderer paints
/// `HIGHLIGHT_BG` under `col_start..=col_end_inclusive` on
/// viewport row `view_row`.  L2 main loop translates a focused
/// search hit's `WirePhysicalSpan` into one or more of these by
/// mapping the hit's row indices through the current `view_offset`
/// (scrollback hit ⇒ row = scrollback_idx - (sb_len - view_offset),
/// live hit ⇒ row = grid-local row offset).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub view_row: u16,
    pub col_start: u16,
    pub col_end_inclusive: u16,
}

/// C4 — per-pane active-highlight slot.  `query_id` lets the
/// renderer drop stale highlights when a fresh `SearchScrollback`
/// supersedes the one whose hit is currently highlighted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveHighlight {
    pub query_id: u32,
    pub spans: Vec<HighlightSpan>,
}

/// C1 — pane attachment that owns per-pane interactive state above
/// the terminal grid: search bar, result list, future tools.  The
/// trait is intentionally minimal in C1 (no `render()` yet); C2 will
/// add a render hook once the search bar gives the framework its
/// first real consumer.  `Send` so the search worker (B3) and tool
/// state can coexist on the L2 main thread without lock contention.
pub trait PaneTool: Send {
    /// Which slot this tool occupies.
    fn slot(&self) -> ToolSlot;
    /// Height in cell rows for fixed slots; ignored for Overlay.
    fn fixed_height_rows(&self) -> u16 {
        0
    }
    /// Hit-test (overlay only): is `(x_phys, y_phys)` inside?
    fn hit_test(&self, _x_phys: f64, _y_phys: f64) -> bool {
        false
    }
    /// Key event while this tool has focus.  Default: Pass.
    fn on_key(&mut self, _ev: &MarspotKeyEvent, _mods: Modifiers) -> InputDisposition {
        InputDisposition::Pass
    }
}

/// Per-session render parameters.  Caller bundles the relevant bits
/// so the renderer doesn't need to know about Session, Marspot, or
/// MarspotEvent — anything that can produce a Grid + view offset can
/// drive a render.
pub struct SessionView<'a> {
    pub grid: &'a Grid,
    /// Lines scrolled up from live (0 = live view).
    pub view_offset: u16,
    /// DECTCEM (?25) — false ⇒ hide cursor for this session.
    pub cursor_visible: bool,
    /// True for the session that owns the keyboard right now.
    /// Drives the focus outline + cursor style (filled vs hollow).
    pub focused: bool,
    /// Short label drawn in the title strip at the top of the
    /// cell.  Empty string skips the strip entirely (useful for
    /// snapshot / mcli single-session rendering).
    pub title: &'a str,
    /// Optional text selection for this session.  Coords are
    /// viewport-local (caller has already projected absolute rows
    /// through the current `view_offset` and clipped to the visible
    /// band).  When `blockwise` is true the renderer fills a
    /// rectangle `[min(anchor.col, focus.col)..=max(...)]` on each
    /// row from anchor to focus; otherwise it paints a row-band:
    /// first row from anchor.col to end, middle rows entirely, last
    /// row from start to focus.col — the iTerm2 default.
    pub selection: Option<SelectionView>,
    /// Active IME preedit ("marked text") at the cursor — empty when
    /// nothing is being composed.  Renderer draws each character at
    /// successive cell positions starting at the cursor, with a
    /// hairline underline so the user can see what the IME hasn't
    /// committed yet.  Only meaningful when this is the focused
    /// session AND view_offset == 0 (preedit is anchored to the live
    /// cursor; scrolled-back views don't show one).
    pub ime_preedit: &'a str,
    /// A deferred per-session silent update is staged for this pane
    /// (target #4 step 5b).  When set on the focused pane, the renderer
    /// draws a refresh affordance at the right edge of the title strip;
    /// clicking it triggers the swap.  Only the focused pane ever carries
    /// it (idle panes swap immediately).
    pub update_pending: bool,
    /// RFC-006 — this slot is a dormant placeholder (a moved-out
    /// pane's empty seat).  The renderer recesses it: a translucent
    /// scrim over the whole cell so it reads as "space held, nothing
    /// running" next to live panes, with the grid's own hint text
    /// showing through.
    pub dormant: bool,
    /// RFC-001 plugin badge — a short tag drawn at the right edge of
    /// the title strip (left of the optional refresh affordance) so
    /// plugins can surface per-pane metadata without overloading the
    /// main title.  Empty = nothing drawn.  See `MsgType::PaneBadge`.
    pub right_badge: &'a str,
    /// C1 — total cell-rows reserved below the title strip for
    /// `ToolSlot::TopFixed` tools.  Sum of each TopFixed tool's
    /// `fixed_height_rows()`.  The renderer shifts the grid inner
    /// rect's top edge down by this many cells.  `0` = no top tools
    /// (same as pre-C1 behaviour, byte-identical render).
    pub top_fixed_h_cells: u16,
    /// C1 — total cell-rows reserved at the bottom of the pane for
    /// `ToolSlot::BottomFixed` tools.  Shrinks the grid inner rect's
    /// bottom edge upward by this many cells.  `0` = no bottom tools.
    pub bot_fixed_h_cells: u16,
    /// C4 — viewport-row-keyed highlight spans for the active search
    /// hit.  Empty slice = no highlight (default).  Renderer paints
    /// `HIGHLIGHT_BG` under each listed `(view_row, col_start..=
    /// col_end_inclusive)`.
    pub highlight_spans: &'a [HighlightSpan],
    /// F1+ — search overlay snapshot.  `None` when the pane has no
    /// active search (the overlay is invisible); `Some` when Cmd+F
    /// has opened the bar.  The renderer draws a query input strip
    /// at the top-right of the pane and an optional result list
    /// below it.  Built per-frame from `pane.search` so any
    /// keystroke that mutates the live state reflects on the next
    /// render.
    pub search_overlay: Option<SearchOverlayView>,
    /// F1+13 — content-freshness tag the renderer uses to short-
    /// circuit per-pane instance rebuilds.  Caller (`Pane::as_view`)
    /// supplies a monotonic-ish counter that bumps whenever the
    /// underlying grid / view state would change the rendered
    /// output.  `0` (default for tests / mcli) disables caching
    /// for that pane.
    pub seq: u64,
}

/// F1+ — immutable per-frame snapshot of the search overlay, fed to
/// the renderer in `SessionView`.  Owned (not `'a`-borrowed) so the
/// renderer doesn't need to keep `pane.search` borrowed across
/// `build_instances`; the clone cost is paid only when search is
/// open (a String per query + per visible snippet, < 1 µs).  Subset
/// of §6.6.5's `SearchOverlay` fields — minimum needed to paint the
/// bar + a minimal result list.
#[derive(Clone, Debug)]
pub struct SearchOverlayView {
    pub query: String,
    /// Char-column of the edit cursor inside `query`.
    pub query_cursor: u16,
    pub case_sensitive: bool,
    /// `(focused_1based, total)` — drawn as `N/M`.  `None` skips the
    /// counter (e.g. before the first SearchResults arrives).
    pub counter: Option<(u32, u32)>,
    /// Up to `VIEWPORT` hit snippets (newest-first).  Each row's
    /// `is_focused` flag drives the focused-row BG highlight.
    pub hits: Vec<SearchHitView>,
}

/// Per-row view of one hit in the result list.
#[derive(Clone, Debug)]
pub struct SearchHitView {
    pub snippet: String,
    pub is_focused: bool,
}

#[derive(Copy, Clone, Debug)]
pub struct SelectionView {
    pub anchor: (u16, u16),
    pub focus: (u16, u16),
    pub blockwise: bool,
}

/// Serialise the text under a selection on `grid` to a clipboard string.
///
/// `anchor`/`focus` are `(col, abs)` where `abs` is rows up from the live
/// bottom (0 = live bottom row, `rows-1` = live top, then `rows..` walk
/// scrollback newest→oldest), matching `Grid::cell_at_view`. The pair is
/// normalised internally so order doesn't matter. `blockwise` carves a
/// rectangle `[min(col)..=max(col)]` on every row; otherwise the iTerm2
/// row-band rule applies (first row from its col to end, middle rows
/// whole, last row from start to its col). Trailing spaces are trimmed
/// per row. Returns `None` when the selection is empty.
///
/// Lives here (not in the GUI `ui` module) so an L3 session process —
/// which owns the real grid + scrollback but no GUI — can answer L2's
/// `GetSelectionText` request with the same logic the in-process panes
/// use. `blockwise: bool` keeps it decoupled from the GUI `SelectionMode`.
pub fn grid_selection_text(
    grid: &Grid,
    anchor: (u16, u32),
    focus: (u16, u32),
    blockwise: bool,
) -> Option<String> {
    let cols = grid.cols();
    let rows = grid.rows();
    if cols == 0 || rows == 0 {
        return None;
    }
    let (a_col, a_abs) = anchor;
    let (f_col, f_abs) = focus;
    // Bigger abs = older = top of the visual selection.
    let (top_col, top_abs, bot_col, bot_abs) = if (a_abs, a_col) >= (f_abs, f_col) {
        (a_col, a_abs, f_col, f_abs)
    } else {
        (f_col, f_abs, a_col, a_abs)
    };
    let block_lo = a_col.min(f_col);
    let block_hi = a_col.max(f_col);
    let last_view_row = rows.saturating_sub(1);
    let mut out = String::new();
    let mut abs = top_abs;
    // Buffer the previous row's raw text (un-finalized) so we can
    // decide what to do with its tail once we see the next row.  If
    // the next row is a DECAWM soft-wrap continuation of this one,
    // the tail is real content (last col was forced to overflow) —
    // we concatenate without a `\n` and without `trim_end`.  Else
    // we treat the row boundary as a logical line break: emit `\n`
    // and `trim_end` the tail (the post-content cells were blank
    // padding the program never wrote into).
    let mut prev_text: Option<String> = None;
    loop {
        if abs > u16::MAX as u32 {
            // Beyond what cell_at_view can address; treat as unreachable.
            if abs == bot_abs {
                break;
            } else {
                abs -= 1;
                continue;
            }
        }
        let (col_lo, col_hi) = if blockwise {
            (block_lo, block_hi)
        } else if top_abs == bot_abs {
            // Single-row selection: anchor and focus sit on the same row, so
            // `top_col`/`bot_col` are just its two ends (ordered by column,
            // not row).  Span min..=max — using the row-spanning rule below
            // would set col_lo=top_col(max)..col_hi=bot_col(min), an empty
            // range, so a one-line copy silently produced nothing.
            (top_col.min(bot_col), top_col.max(bot_col))
        } else {
            let lo = if abs == top_abs { top_col } else { 0 };
            let hi = if abs == bot_abs { bot_col } else { cols.saturating_sub(1) };
            (lo, hi)
        };
        let mut row_text = String::new();
        for c in col_lo..=col_hi {
            if c >= cols {
                break;
            }
            let cell = grid.cell_at_view(abs as u16, c, last_view_row);
            // NUL is the wide-char trail-half sentinel — skip it so CJK
            // doesn't paste with an extra space per wide glyph.
            if cell.ch == '\0' {
                continue;
            }
            row_text.push(cell.ch);
        }
        // Soft-wrap merge gate.  The wrap flag is "this row is the
        // overflow continuation of the row above," i.e. the row
        // above's col=cols-1 was filled and the parser pushed the
        // next char down.  Two facts follow:
        //   * the prev row's tail chars are real content (don't
        //     `trim_end` them, even if some are real spaces)
        //   * there is no logical newline between prev and current
        //     (don't insert `\n`)
        // Neither fact depends on which COLUMNS of the current row
        // the user happens to have selected — only on the wrap flag
        // itself.  Blockwise mode is the lone exception: in a column
        // band, the "previous row" we buffered is the band of the
        // row above, not a logical line, so wrap merging would glue
        // unrelated cells.  Earlier this gate also demanded col_lo=0
        // / col_hi=cols-1 on both rows; that was wrong (the bot row
        // of a multi-line selection routinely stops mid-row, so the
        // wrap merge silently fell through to `\n + trim_end`,
        // dropping the prev tail and inserting a fake newline).
        let current_is_continuation =
            !blockwise && grid.wrapped_at_view(abs as u16, last_view_row);
        if let Some(prev) = prev_text.take() {
            if current_is_continuation {
                // Preserve every cell of prev — its last column was
                // forced to overflow into this row, so its trailing
                // chars are real content, not padding.
                out.push_str(&prev);
            } else {
                out.push_str(prev.trim_end());
                out.push('\n');
            }
        }
        prev_text = Some(row_text);
        if abs == bot_abs {
            break;
        }
        abs -= 1;
    }
    // Final row of the selection: always trim — its right edge is the
    // end of the user's drag, not content the program is going to
    // continue onto another row.
    if let Some(prev) = prev_text {
        out.push_str(prev.trim_end());
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod selection_tests {
    use super::grid_selection_text;
    use crate::grid::{Cell, Grid};

    fn write_row(grid: &mut Grid, row: u16, s: &str) {
        for (c, ch) in s.chars().enumerate() {
            grid.set_cell(c as u16, row, Cell { ch, ..Default::default() });
        }
    }

    // cell_at_view maps abs 0 → bottom grid row (rows-1).  grid_selection_text
    // calls it with viewport_row = rows-1, so a selection abs N reads grid row
    // rows-1-N.  Put text on the bottom row → abs 0.
    #[test]
    fn single_row_selection_spans_columns() {
        let mut grid = Grid::new(10, 3);
        write_row(&mut grid, 2, "hello");
        // anchor/focus on the same row (abs 0), columns 0..=4.  Either order
        // of the two ends must yield the same span — this is the case that
        // used to collapse to an empty range and copy nothing.
        assert_eq!(
            grid_selection_text(&grid, (0, 0), (4, 0), false).as_deref(),
            Some("hello")
        );
        assert_eq!(
            grid_selection_text(&grid, (4, 0), (0, 0), false).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn multi_row_selection_joins_with_newline() {
        let mut grid = Grid::new(10, 3);
        write_row(&mut grid, 1, "foo");
        write_row(&mut grid, 2, "bar");
        // Top row (abs 1) from its start, bottom row (abs 0) through col 2.
        assert_eq!(
            grid_selection_text(&grid, (0, 1), (2, 0), false).as_deref(),
            Some("foo\nbar")
        );
    }

    // Phase 1 — DECAWM soft-wrap aware selection.  A row that the
    // parser flagged as a continuation of the row above (`row_wrapped`
    // set) is merged into the previous row without an intervening `\n`,
    // and the previous row keeps its trailing chars (no trim) because
    // they were real content forced down by the overflow.
    #[test]
    fn soft_wrap_continuation_merges_without_newline() {
        let mut grid = Grid::new(5, 3);
        // Top row (abs 2) fills col 0..=4 with "hello"; the parser
        // flagged the row BELOW as the wrap continuation.
        write_row(&mut grid, 0, "hello");
        // Continuation row (abs 1) carries "world" at cols 0..=4.
        write_row(&mut grid, 1, "world");
        // The full-width content row (abs 0) is empty (logical line
        // ends after "helloworld").
        // Mark the wrap chain: rows 1 and 2 (live row indices) are
        // continuations of the row physically above.  set_row_wrapped
        // takes live-row coords (0 = top live row).
        grid.set_row_wrapped(1, true);
        // Select across both wrapped rows.  Expected: merged into a
        // single token "helloworld" with no `\n`.
        assert_eq!(
            grid_selection_text(&grid, (0, 2), (4, 1), false).as_deref(),
            Some("helloworld")
        );
    }

    // Regression for the "L3_EXECV_INVOKE log 里 control_stream_fd=N…
    // RESUMED" copy-paste case (2026-06-17): user selects across two
    // soft-wrapped rows, but the bot row ends mid-row.  Pre-fix, the
    // wrap-merge required col_hi=cols-1 on the current row, so the
    // bot row failed the gate, the merge fell through, the prev row
    // was trim_end + `\n` joined — user saw a phantom newline and
    // lost the prev row's trailing chars.
    #[test]
    fn soft_wrap_merges_when_bot_row_ends_mid_row() {
        let mut grid = Grid::new(5, 3);
        write_row(&mut grid, 0, "hello");
        write_row(&mut grid, 1, "world");
        grid.set_row_wrapped(1, true);
        // Select hello..wor (top from col 0, bot up to col 2 only).
        assert_eq!(
            grid_selection_text(&grid, (0, 2), (2, 1), false).as_deref(),
            Some("hellowor")
        );
    }

    // Same wrap-merge semantics for a TOP row that starts mid-row.
    #[test]
    fn soft_wrap_merges_when_top_row_starts_mid_row() {
        let mut grid = Grid::new(5, 3);
        write_row(&mut grid, 0, "hello");
        write_row(&mut grid, 1, "world");
        grid.set_row_wrapped(1, true);
        // Select llo + world (top from col 2 onwards).
        assert_eq!(
            grid_selection_text(&grid, (2, 2), (4, 1), false).as_deref(),
            Some("lloworld")
        );
    }

    // Trailing real spaces inside a wrapped row must NOT be trim'd:
    // they're real content that overflowed.
    #[test]
    fn soft_wrap_preserves_trailing_spaces_in_prev_row() {
        let mut grid = Grid::new(5, 3);
        // "ab " + "cd" on the row above; the trailing space is real
        // (parser would only wrap if col=cols-1 was occupied).  For
        // this scenario assume col 2 is a real space, col 3+4 are
        // "ab" – so write_row writes "ab cd" across 5 cols.
        write_row(&mut grid, 0, "ab cd");
        write_row(&mut grid, 1, "next!");
        grid.set_row_wrapped(1, true);
        // Whole selection.
        assert_eq!(
            grid_selection_text(&grid, (0, 2), (4, 1), false).as_deref(),
            Some("ab cdnext!")
        );
    }

    /// End-to-end test: feed real bytes through the VT parser, then
    /// run `grid_selection_text` against the resulting grid.  Mirrors
    /// the actual L3 pipeline (PTY byte stream → Terminal::feed →
    /// grid → on Cmd-C reply with grid_selection_text), so a fix in
    /// render.rs that passes unit tests but fails the live pipeline
    /// gets caught here instead of by the user copy-pasting.
    fn select_text_via_parser(cols: u16, rows: u16, bytes: &[u8]) -> Option<String> {
        let mut t = crate::terminal::Terminal::new(cols, rows);
        t.feed(bytes);
        let grid = t.grid();
        // Whole visible viewport: (col 0, abs rows-1) → (cols-1, abs 0).
        let top_abs = (rows - 1) as u32;
        let bot_abs = 0u32;
        grid_selection_text(
            grid,
            (0, top_abs),
            (cols - 1, bot_abs),
            false,
        )
    }

    #[test]
    fn e2e_soft_wrap_long_ascii_has_no_phantom_newline() {
        // 200 'a's at 20×3 forces 10 rows of soft-wrap (well over the
        // viewport).  Grid's bottom 3 rows hold the tail; the wrap
        // continuation flag is set by the parser on every row after
        // the first overflow.  Whole-viewport copy must produce a
        // contiguous run of 'a's with no `\n` interrupting it.
        let bytes = "a".repeat(200);
        let out = select_text_via_parser(20, 3, bytes.as_bytes())
            .expect("non-empty selection");
        assert!(!out.contains('\n'), "phantom \\n in: {out:?}");
        assert!(out.chars().all(|c| c == 'a'), "non-'a' char in: {out:?}");
    }

    #[test]
    fn e2e_hard_newline_inserts_logical_break() {
        // Three short lines separated by CR/LF — the parser sets no
        // wrap flag, so each line boundary is a real logical newline
        // and selection should preserve it.  Validates the negative
        // case so we don't accidentally swallow real newlines along
        // with the soft-wrap fix.
        let bytes = b"abc\r\ndef\r\nghi";
        let out = select_text_via_parser(20, 3, bytes)
            .expect("non-empty selection");
        assert_eq!(out, "abc\ndef\nghi");
    }

    #[test]
    fn e2e_long_url_across_soft_wrap_is_one_logical_line() {
        // URL that overflows the column width; selection must yield
        // the URL un-broken — same input grid_links::scan_visible_
        // links uses to detect the multi-row LinkRange.
        let cols = 20u16;
        let url = "https://example.com/some/very/long/path/that/wraps?q=value";
        let out = select_text_via_parser(cols, 5, url.as_bytes())
            .expect("non-empty selection");
        let normalised: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(normalised, url, "URL mangled by wrap: {out:?}");
    }

    #[test]
    fn logical_newline_still_breaks() {
        let mut grid = Grid::new(10, 3);
        // Same row-mapping convention as `multi_row_selection_joins_
        // _with_newline`: write into live rows 1+2 so they map to abs
        // 1 (top) and abs 0 (bottom) under last_view_row=2.
        write_row(&mut grid, 1, "abc");
        write_row(&mut grid, 2, "def");
        // No wrap flag set — these are two logical lines, joined with `\n`.
        assert_eq!(
            grid_selection_text(&grid, (0, 1), (2, 0), false).as_deref(),
            Some("abc\ndef")
        );
    }
}

/// One row of the sidebar — what the user sees on the left.  Length
/// of the slice passed to `MetalRenderer::render_layout` should match
/// `views.len()`; entry `i` describes session `i`.
pub struct SidebarEntry<'a> {
    /// Short label drawn after the state dot, e.g. `"1"`, `"build"`.
    pub label: &'a str,
    /// Drives the colour of the state dot.
    pub state: SessionState,
}

// ─── Box-drawing arms ─────────────────────────────────────────────────────

/// Arm-direction bits encoded in a u8: N|S|W|E (4 LSBs). Each box-drawing
/// character is a combination of these arms emanating from the cell
/// centre. `render_metal` interprets the bits when rasterising the
/// per-cell mask.
pub(crate) const ARM_N: u8 = 0b0001;
pub(crate) const ARM_S: u8 = 0b0010;
pub(crate) const ARM_W: u8 = 0b0100;
pub(crate) const ARM_E: u8 = 0b1000;

/// If `ch` is a box-drawing character we paint programmatically, return
/// its arm composition.  Covers the common subset of U+2500..U+257F
/// that TUIs (claudecode welcome panel, htop, ncurses dialogs) use to
/// draw frames — single-line corners, tees, crosses. Heavy / double
/// variants intentionally fall back to the font for now.
///
/// Rounded corners ╭ ╮ ╯ ╰ (U+256D-U+2570) are aliased to the matching
/// sharp corners; in cell-local pixel rendering "rounded" and "sharp"
/// only differ at sub-pixel curvature we don't model, so visually they
/// render identical and importantly join the adjacent `─` / `│` strokes
/// without seams — which the font path would NOT do.
pub fn box_drawing_arms(ch: char) -> Option<u8> {
    Some(match ch {
        '\u{2500}' | '\u{2501}' => ARM_W | ARM_E, // ─ ━
        '\u{2502}' | '\u{2503}' => ARM_N | ARM_S, // │ ┃
        '\u{250C}' | '\u{250D}' | '\u{250E}' | '\u{250F}' => ARM_E | ARM_S, // ┌ variants
        '\u{2510}' | '\u{2511}' | '\u{2512}' | '\u{2513}' => ARM_W | ARM_S, // ┐ variants
        '\u{2514}' | '\u{2515}' | '\u{2516}' | '\u{2517}' => ARM_E | ARM_N, // └ variants
        '\u{2518}' | '\u{2519}' | '\u{251A}' | '\u{251B}' => ARM_W | ARM_N, // ┘ variants
        '\u{251C}'..='\u{2523}' => ARM_N | ARM_S | ARM_E, // ├ variants
        '\u{2524}'..='\u{252B}' => ARM_N | ARM_S | ARM_W, // ┤ variants
        '\u{252C}'..='\u{2533}' => ARM_W | ARM_E | ARM_S, // ┬ variants
        '\u{2534}'..='\u{253B}' => ARM_W | ARM_E | ARM_N, // ┴ variants
        '\u{253C}'..='\u{254B}' => ARM_W | ARM_E | ARM_N | ARM_S, // ┼ variants
        '\u{256D}' => ARM_E | ARM_S, // ╭ rounded top-left  ≡ ┌
        '\u{256E}' => ARM_W | ARM_S, // ╮ rounded top-right ≡ ┐
        '\u{256F}' => ARM_W | ARM_N, // ╯ rounded bot-right ≡ ┘
        '\u{2570}' => ARM_E | ARM_N, // ╰ rounded bot-left  ≡ └
        _ => return None,
    })
}

/// Write box-drawing arms directly into a w×h grayscale byte buffer in
/// y-down image-natural orientation (row 0 = top). Uses kitty's exact
/// formula: integer midline + stroke run `[mid - t/2, mid - t/2 + t)`.
/// Each arm extends past the centerline by `half_t_hi` into the
/// perpendicular arm's column so the corner overlap region is fully
/// covered with no notch.
pub fn rasterize_arms_into_buf(buf: &mut [u8], w: usize, h: usize, arms: u8) {
    // Stroke thickness — kitty/alacritty's `max(1, round(cell_w/8))`.
    let t = (((w as f32) / 8.0).round() as i32).max(1) as usize;
    let half_t_lo = t / 2; // integer floor
    let half_t_hi = t - half_t_lo; // integer ceil — equals half_t_lo for even t, +1 for odd
    let mid_x = w / 2; // integer midline; consistent across cells of any width
    let mid_y = h / 2;
    // Stroke runs are exactly `t` rows / columns wide (kitty's
    // `start + stroke`, never re-derived from `center ± t/2` → no
    // off-by-one between even / odd `t` and even / odd cell dims).
    let stroke_x0 = mid_x.saturating_sub(half_t_lo);
    let stroke_y0 = mid_y.saturating_sub(half_t_lo);
    let stroke_x1 = (stroke_x0 + t).min(w);
    let stroke_y1 = (stroke_y0 + t).min(h);

    let fill = |buf: &mut [u8], x0: usize, x1: usize, y0: usize, y1: usize| {
        let x0 = x0.min(w);
        let x1 = x1.min(w);
        let y0 = y0.min(h);
        let y1 = y1.min(h);
        for y in y0..y1 {
            let row_start = y * w;
            buf[row_start + x0..row_start + x1].fill(0xff);
        }
    };

    // In image y-down: ARM_N = top of cell (low y), ARM_S = bottom (high y).
    // Each arm extends past midline by `half_t_hi` into the perpendicular
    // arm's column — this is what fills the corner pocket.
    if arms & ARM_W != 0 {
        fill(buf, 0, mid_x + half_t_hi, stroke_y0, stroke_y1);
    }
    if arms & ARM_E != 0 {
        fill(buf, stroke_x0, w, stroke_y0, stroke_y1);
    }
    if arms & ARM_N != 0 {
        fill(buf, stroke_x0, stroke_x1, 0, mid_y + half_t_hi);
    }
    if arms & ARM_S != 0 {
        fill(buf, stroke_x0, stroke_x1, stroke_y0, h);
    }
}

// ─── Block elements (U+2580..U+259F) ──────────────────────────────────────

/// One filled rectangle inside the cell, in 8-unit (1/8-of-cell)
/// coordinates. CG y-up convention: `y_bot_8` is at the bottom of the
/// cell (SCREEN bottom), `y_top_8` is at the top. The rasteriser flips
/// to y-down image orientation when writing to the mask buffer.
#[derive(Clone, Copy)]
pub struct BlockRect {
    pub x_left_8: u8,
    pub y_bot_8: u8,
    pub x_right_8: u8,
    pub y_top_8: u8,
}

#[derive(Clone, Copy)]
pub struct BlockShape {
    /// Up to 2 filled rectangles per shape — quadrant chars like
    /// `▙` need two rects to form an L; `█` and friends need just one.
    pub rects: [Option<BlockRect>; 2],
    /// Alpha for shaded variants ░ ▒ ▓; opaque (1.0) for the rest.
    pub alpha: f64,
}

const fn r(x_left_8: u8, y_bot_8: u8, x_right_8: u8, y_top_8: u8) -> Option<BlockRect> {
    Some(BlockRect {
        x_left_8,
        y_bot_8,
        x_right_8,
        y_top_8,
    })
}

const fn one(rect: Option<BlockRect>) -> BlockShape {
    BlockShape {
        rects: [rect, None],
        alpha: 1.0,
    }
}

const fn two(a: Option<BlockRect>, b: Option<BlockRect>) -> BlockShape {
    BlockShape {
        rects: [a, b],
        alpha: 1.0,
    }
}

const fn shaded(alpha: f64) -> BlockShape {
    BlockShape {
        rects: [r(0, 0, 8, 8), None],
        alpha,
    }
}

/// U+2580..U+259F block elements (lower/upper N/8, side N/8, quadrants,
/// shaded). Used by claudecode for the pixel-art welcome icon and by
/// progress bars, sparklines, etc. The font's glyphs for these don't
/// span the cell so we paint them directly like box-drawing chars.
pub fn block_element_rects(ch: char) -> Option<BlockShape> {
    Some(match ch {
        '\u{2580}' => one(r(0, 4, 8, 8)),                 // ▀ upper half
        '\u{2581}' => one(r(0, 0, 8, 1)),                 // ▁ lower 1/8
        '\u{2582}' => one(r(0, 0, 8, 2)),                 // ▂ lower 2/8
        '\u{2583}' => one(r(0, 0, 8, 3)),                 // ▃
        '\u{2584}' => one(r(0, 0, 8, 4)),                 // ▄ lower half
        '\u{2585}' => one(r(0, 0, 8, 5)),                 // ▅
        '\u{2586}' => one(r(0, 0, 8, 6)),                 // ▆
        '\u{2587}' => one(r(0, 0, 8, 7)),                 // ▇
        '\u{2588}' => one(r(0, 0, 8, 8)),                 // █ full
        '\u{2589}' => one(r(0, 0, 7, 8)),                 // ▉ left 7/8
        '\u{258A}' => one(r(0, 0, 6, 8)),                 // ▊
        '\u{258B}' => one(r(0, 0, 5, 8)),                 // ▋
        '\u{258C}' => one(r(0, 0, 4, 8)),                 // ▌ left half
        '\u{258D}' => one(r(0, 0, 3, 8)),                 // ▍
        '\u{258E}' => one(r(0, 0, 2, 8)),                 // ▎
        '\u{258F}' => one(r(0, 0, 1, 8)),                 // ▏
        '\u{2590}' => one(r(4, 0, 8, 8)),                 // ▐ right half
        '\u{2591}' => shaded(0.25),                       // ░ light shade
        '\u{2592}' => shaded(0.50),                       // ▒ medium shade
        '\u{2593}' => shaded(0.75),                       // ▓ dark shade
        '\u{2594}' => one(r(0, 7, 8, 8)),                 // ▔ upper 1/8
        '\u{2595}' => one(r(7, 0, 8, 8)),                 // ▕ right 1/8
        '\u{2596}' => one(r(0, 0, 4, 4)),                 // ▖ lower-left quadrant
        '\u{2597}' => one(r(4, 0, 8, 4)),                 // ▗ lower-right
        '\u{2598}' => one(r(0, 4, 4, 8)),                 // ▘ upper-left
        '\u{2599}' => two(r(0, 4, 4, 8), r(0, 0, 8, 4)),  // ▙ UL + lower half
        '\u{259A}' => two(r(0, 4, 4, 8), r(4, 0, 8, 4)),  // ▚ UL + LR
        '\u{259B}' => two(r(0, 4, 8, 8), r(0, 0, 4, 4)),  // ▛ upper half + LL
        '\u{259C}' => two(r(0, 4, 8, 8), r(4, 0, 8, 4)),  // ▜ upper half + LR
        '\u{259D}' => one(r(4, 4, 8, 8)),                 // ▝ upper-right
        '\u{259E}' => two(r(4, 4, 8, 8), r(0, 0, 4, 4)),  // ▞ UR + LL
        '\u{259F}' => two(r(4, 4, 8, 8), r(0, 0, 8, 4)),  // ▟ UR + lower half
        _ => return None,
    })
}

/// Write block-element shape directly into a w×h grayscale byte buffer
/// in y-down image orientation. BlockRect coords are in CG y-up eighths
/// (the same units `block_element_rects` produces), so we flip the y
/// component when computing image rows.
pub fn rasterize_block_into_buf(buf: &mut [u8], w: usize, h: usize, shape: BlockShape) {
    let fill_val = if shape.alpha < 1.0 {
        (255.0 * shape.alpha) as u8
    } else {
        0xff
    };
    for opt_r in shape.rects.iter() {
        let Some(r) = opt_r else { continue };
        let x0 = (w * r.x_left_8 as usize) / 8;
        let x1 = (w * r.x_right_8 as usize) / 8;
        // BlockRect is in CG y-up eighths: y_bot_8 = bottom in CG = SCREEN bottom
        // = HIGH image y. y_top_8 = top in CG = SCREEN top = LOW image y.
        let y0_img = (h * (8 - r.y_top_8 as usize)) / 8;
        let y1_img = (h * (8 - r.y_bot_8 as usize)) / 8;
        let x0 = x0.min(w);
        let x1 = x1.min(w);
        let y0 = y0_img.min(h);
        let y1 = y1_img.min(h);
        for y in y0..y1 {
            let row_start = y * w;
            buf[row_start + x0..row_start + x1].fill(fill_val);
        }
    }
}
