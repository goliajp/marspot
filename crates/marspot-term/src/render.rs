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
use crate::session::SessionState;

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
    /// RFC-001 plugin badge — a short tag drawn at the right edge of
    /// the title strip (left of the optional refresh affordance) so
    /// plugins can surface per-pane metadata without overloading the
    /// main title.  Empty = nothing drawn.  See `MsgType::PaneBadge`.
    pub right_badge: &'a str,
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
    let mut first = true;
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
        if !first {
            out.push('\n');
        }
        out.push_str(row_text.trim_end());
        first = false;
        if abs == bot_abs {
            break;
        }
        abs -= 1;
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
