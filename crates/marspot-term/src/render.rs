// CARVE-OUT: pre-existing file: render data types and box-drawing helpers; shrink on every touch
//! Render-layer data types + box-drawing helpers.
//!
//! This module used to host an AppKit/CGContext-based `Renderer`
//! alongside the Metal one in `render_metal.rs`.  Everything now goes
//! through `MetalRenderer` and the AppKit renderer was removed — what
//! remains here is the shared part:
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
/// do not affect grid layout.
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
    /// snapshot rendering).
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
    /// RFC-006 — this slot is a dormant placeholder (a moved-out
    /// pane's empty seat).  The renderer recesses it: a translucent
    /// scrim over the whole cell so it reads as "space held, nothing
    /// running" next to live panes, with the grid's own hint text
    /// showing through.
    pub dormant: bool,
    /// How far this pane has receded from active use: 0 live,
    /// 1 resting (a bound session that finished its turn a while ago),
    /// 2 parked (its program reclaimed, picture frozen).
    ///
    /// A level, not a shade: how dark each one looks is the
    /// renderer's business, because it also depends on whether this
    /// is the pane the user is in — which L1 does not know and should
    /// not have to.
    ///
    /// Separate from `dormant` ("an empty seat left by a pane that
    /// moved out") because they are different facts that happen to
    /// share a scrim.  Both can be true; the deeper one wins.
    pub recede: u32,
    /// The dim to actually paint, already eased.
    ///
    /// `recede` is the *step* the pane is on; this is where the picture
    /// has got to on its way there.  The renderer takes the number
    /// rather than deriving it, because easing needs memory between
    /// frames and the renderer has none.
    pub scrim: f32,
    /// RFC-001 plugin badge — a short tag drawn at the right edge of
    /// the title strip so plugins can surface per-pane metadata without overloading the
    /// main title.  Empty = nothing drawn.  See `MsgType::PaneBadge`.
    pub right_badge: &'a str,
    /// Is this pane painted by an agent TUI that wraps its own lines?
    ///
    /// Such a program renders a URL or path to a fixed inner width and
    /// starts the remainder on a new line with a hanging indent — no
    /// DECAWM wrap flag, because it never let the terminal wrap.  The
    /// link scanner has to be told, or every such path is cut at the
    /// break (2026-09-06 field report, inside codex).
    ///
    /// It used to be inferred from "the badge is non-empty", which is
    /// the wrong fact: a plugin whose badge text momentarily comes out
    /// empty still owns the pane, and the links would silently start
    /// truncating.  A wheel-key declaration is the durable assertion —
    /// a plugin only makes it about a program it is driving.
    pub agent_tui: bool,
    /// The pane's working directory, when known.  A relative path on
    /// screen (`src/main.rs`) only becomes a link when there is a
    /// directory to resolve it against; Open and Copy then act on the
    /// full path while the underline stays over what is drawn.
    pub cwd: &'a str,
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
    /// How far into its history the pane is showing, for the
    /// scrollbar.  Separate from `view_offset`: an L3 pane draws its
    /// mirror at offset 0 while showing the rows L3 published at the
    /// requested offset, and this is that requested offset.
    pub history: HistoryView,
    /// The session whose grid this is.  A pane can be given another
    /// session's, of the same size and at the same `seq`.
    pub session: u64,
    /// What is drawn in the rows the pane gives up at its bottom.
    pub strip: Option<StripView<'a>>,
    /// A sentence to show over the top of the pane until it is taken
    /// away; see `MsgType::Notice`.
    pub notice: Option<&'a str>,
    /// F1+13 — content-freshness tag the renderer uses to short-
    /// circuit per-pane instance rebuilds.  Caller (`Pane::as_view`)
    /// supplies a monotonic-ish counter that bumps whenever the
    /// underlying grid / view state would change the rendered
    /// output.  `0` (default for tests) disables caching
    /// for that pane.
    pub seq: u64,
}

mod view_parts;
pub use view_parts::{ActiveHighlight, HighlightSpan, HistoryView, SearchHitView, SearchOverlayView, StripView};

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
    // One row's worth of cluster text, refilled per row.  Scrollback
    // cells hold the base codepoint, so what a cluster said is keyed by
    // where it was -- asked once for the row, not once per cell.
    let mut row_clusters: Vec<(u16, String)> = Vec::new();
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
            let hi = if abs == bot_abs {
                bot_col
            } else {
                cols.saturating_sub(1)
            };
            (lo, hi)
        };
        let mut row_text = String::new();
        grid.row_clusters_at_view(abs as u16, last_view_row, &mut row_clusters);
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
            // A cell holding a cluster keeps its text in the grid's
            // pool and its `ch` is the index — copying that would put
            // a plane-15 codepoint on the clipboard.
            match grid
                .cluster_text(&cell)
                .or_else(|| crate::grid::cluster_in_row(&row_clusters, c))
            {
                Some(text) => row_text.push_str(text),
                None => row_text.push(cell.ch),
            }
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
        let current_is_continuation = !blockwise && grid.wrapped_at_view(abs as u16, last_view_row);
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
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod selection_tests;

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
        '\u{256D}' => ARM_E | ARM_S,              // ╭ rounded top-left  ≡ ┌
        '\u{256E}' => ARM_W | ARM_S,              // ╮ rounded top-right ≡ ┐
        '\u{256F}' => ARM_W | ARM_N,              // ╯ rounded bot-right ≡ ┘
        '\u{2570}' => ARM_E | ARM_N,              // ╰ rounded bot-left  ≡ └
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
        '\u{2580}' => one(r(0, 4, 8, 8)),                // ▀ upper half
        '\u{2581}' => one(r(0, 0, 8, 1)),                // ▁ lower 1/8
        '\u{2582}' => one(r(0, 0, 8, 2)),                // ▂ lower 2/8
        '\u{2583}' => one(r(0, 0, 8, 3)),                // ▃
        '\u{2584}' => one(r(0, 0, 8, 4)),                // ▄ lower half
        '\u{2585}' => one(r(0, 0, 8, 5)),                // ▅
        '\u{2586}' => one(r(0, 0, 8, 6)),                // ▆
        '\u{2587}' => one(r(0, 0, 8, 7)),                // ▇
        '\u{2588}' => one(r(0, 0, 8, 8)),                // █ full
        '\u{2589}' => one(r(0, 0, 7, 8)),                // ▉ left 7/8
        '\u{258A}' => one(r(0, 0, 6, 8)),                // ▊
        '\u{258B}' => one(r(0, 0, 5, 8)),                // ▋
        '\u{258C}' => one(r(0, 0, 4, 8)),                // ▌ left half
        '\u{258D}' => one(r(0, 0, 3, 8)),                // ▍
        '\u{258E}' => one(r(0, 0, 2, 8)),                // ▎
        '\u{258F}' => one(r(0, 0, 1, 8)),                // ▏
        '\u{2590}' => one(r(4, 0, 8, 8)),                // ▐ right half
        '\u{2591}' => shaded(0.25),                      // ░ light shade
        '\u{2592}' => shaded(0.50),                      // ▒ medium shade
        '\u{2593}' => shaded(0.75),                      // ▓ dark shade
        '\u{2594}' => one(r(0, 7, 8, 8)),                // ▔ upper 1/8
        '\u{2595}' => one(r(7, 0, 8, 8)),                // ▕ right 1/8
        '\u{2596}' => one(r(0, 0, 4, 4)),                // ▖ lower-left quadrant
        '\u{2597}' => one(r(4, 0, 8, 4)),                // ▗ lower-right
        '\u{2598}' => one(r(0, 4, 4, 8)),                // ▘ upper-left
        '\u{2599}' => two(r(0, 4, 4, 8), r(0, 0, 8, 4)), // ▙ UL + lower half
        '\u{259A}' => two(r(0, 4, 4, 8), r(4, 0, 8, 4)), // ▚ UL + LR
        '\u{259B}' => two(r(0, 4, 8, 8), r(0, 0, 4, 4)), // ▛ upper half + LL
        '\u{259C}' => two(r(0, 4, 8, 8), r(4, 0, 8, 4)), // ▜ upper half + LR
        '\u{259D}' => one(r(4, 4, 8, 8)),                // ▝ upper-right
        '\u{259E}' => two(r(4, 4, 8, 8), r(0, 0, 4, 4)), // ▞ UR + LL
        '\u{259F}' => two(r(4, 4, 8, 8), r(0, 0, 8, 4)), // ▟ UR + lower half
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
