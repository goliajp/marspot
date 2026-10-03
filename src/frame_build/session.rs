//! One pane, laid out as instances: the background, the title strip,
//! the grid row by row, and the marks drawn over it.

use crate::font_cache::{resolve_attrs, FontCache, BG};
use crate::frame_build::glyph_resolve::{resolve_cell_glyph_routed, resolve_cluster_glyph};
use crate::frame_build::palette::*;
use crate::frame_build::text_run::preedit_placements;
use crate::glyph_atlas::{GlyphAtlas, SlotMetrics};
use crate::grid::Grid;
use crate::layout::CellRect;
use crate::render::SessionView;
use crate::render_metal::{rgba8_of_f32, CellInstance, GlyphInstance, UiRectInstance};
use crate::frame_build::session_marks::*;

/// What every part of a pane's drawing reads: the view, where its grid
/// sits, and which cells something else covers.
pub(crate) struct PaneGeom<'a> {
    pub(crate) view: &'a SessionView<'a>,
    pub(crate) grid: &'a Grid,
    pub(crate) cols: usize,
    /// Rounded to whole pixels once, so every column advance is exact.
    pub(crate) cell_w: f32,
    pub(crate) cell_h: f32,
    pub(crate) ascent: f32,
    pub(crate) atlas_w: f32,
    pub(crate) atlas_h: f32,
    pub(crate) color_atlas_w: f32,
    pub(crate) color_atlas_h: f32,
    /// Top-left of the terminal content area.
    pub(crate) inner_x: f32,
    pub(crate) inner_y: f32,
    pub(crate) window_focused: bool,
    /// `(row_start, row_end_exclusive, col_start, col_end_inclusive)`
    /// under the search overlay, when it is open.
    pub(crate) overlay_mask: Option<(u16, u16, u16, u16)>,
    /// Where each cluster of an IME composition goes.
    pub(crate) preedit_cells: Vec<(u16, u16, u16, &'a str)>,
    pub(crate) links: Vec<marspot_term::grid_links::LinkRange>,
}

impl PaneGeom<'_> {
    /// Whether the search overlay covers this cell.
    pub(crate) fn under_overlay(&self, row: u16, col: u16) -> bool {
        match self.overlay_mask {
            Some((r0, r1, c0, c1)) => row >= r0 && row < r1 && col >= c0 && col <= c1,
            None => false,
        }
    }

    /// Whether an IME composition covers this cell.
    pub(crate) fn under_preedit(&self, row: u16, col: u16) -> bool {
        !self.preedit_cells.is_empty()
            && self
                .preedit_cells
                .iter()
                .any(|&(r, c, w, _)| r == row && col >= c && col < c + w)
    }

    /// Whether this cell is part of a detected link.
    pub(crate) fn in_link(&self, row: u16, col: u16) -> bool {
        self.links.iter().any(|l| l.row == row && col >= l.col_start && col <= l.col_end)
    }
}

/// What a pane's drawing writes into.
pub(crate) struct PaneOut<'a> {
    pub(crate) font: &'a mut FontCache,
    pub(crate) atlas: &'a mut GlyphAtlas,
    pub(crate) color_atlas: &'a mut GlyphAtlas,
    pub(crate) cells: &'a mut Vec<CellInstance>,
    pub(crate) glyphs: &'a mut Vec<GlyphInstance>,
    pub(crate) color_glyphs: &'a mut Vec<GlyphInstance>,
}

/// One pane, laid out: its background, title strip, grid, selection,
/// links, cursor and IME composition, in that order.
pub(crate) fn push_session(
    rect: &CellRect,
    view: &SessionView,
    window_focused: bool,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    _ui_rects: &mut Vec<UiRectInstance>,
    gutter: f32,
    padding: f32,
    title_h: f32,
) {
    let (color_atlas_w, color_atlas_h) = color_atlas.dims();
    let color_atlas_w = color_atlas_w as f32;
    let color_atlas_h = color_atlas_h as f32;
    // Cell-rect BG: BG_FOCUSED (deeper) for the active pane,
    // BG_PANEL for everyone else.  The DROP into deeper black is
    // the focus indicator — focused reads as "the canvas I'm
    // typing into", surrounding cells stay at the chrome tone.
    let pane_focused = view.focused && window_focused;
    let pane_bg = if pane_focused {
        [BG_FOCUSED.0, BG_FOCUSED.1, BG_FOCUSED.2, 1.0]
    } else {
        [BG_PANEL.0, BG_PANEL.1, BG_PANEL.2, 1.0]
    };
    cells.push(CellInstance {
        origin: [rect.x as f32, rect.y_top as f32],
        size: [rect.w as f32, rect.h as f32],
        color: rgba8_of_f32(pane_bg),
    });

    paint_title_strip(
        rect, view, pane_focused, cell_w, cell_h, ascent, atlas_w, atlas_h, font, atlas, cells,
        glyphs, gutter, padding, title_h,
    );

    let grid = view.grid;
    let cols = grid.cols() as usize;
    // Origin of the terminal content area — inside cell, below
    // the title strip, then padded.  Round both origin and cell_w
    // to integer pixels here ONCE so every `c * cell_w` advance is
    // an exact integer multiple. Without this, fractional cell_w
    // makes adjacent cells round to different stride lengths
    // (col 1 advances by 8, col 2 by 9) while the atlas slot is
    // a fixed width, leaving 1-px seam gaps every few columns in
    // horizontal box-drawing runs.
    let cell_w = cell_w.round();
    let cell_h = cell_h.round();
    let inner_x = (rect.x as f32 + padding).round();
    // C1 — `TopFixed` tools (search bar etc.) push the grid's
    // top edge down by `top_fixed_h_cells * cell_h`.  Empty
    // `tools` ⇒ `top_fixed_h_cells == 0` ⇒ render byte-identical
    // to pre-C1.  `bot_fixed_h_cells` reserves space at the
    // bottom for `BottomFixed` tools — currently informational
    // (no `BottomFixed` consumer until a future tool needs it);
    // the L3 already sizes its grid so renderable rows fit.
    let top_fixed_h = view.top_fixed_h_cells as f32 * cell_h;
    let inner_y = (rect.y_top as f32 + title_h + padding + top_fixed_h).round();

    // F1++ — overlay glyph mask.  Metal renders BG and FG in two
    // separate passes (BG first, then glyphs).  Without this mask
    // grid glyphs under the search bar / list FG-pass straight onto
    // the opaque overlay BG painted in the BG pass, so the underlying
    // terminal text "bleeds through" the chrome.  Pre-compute the
    // overlay's covered cell range here; the per-cell loops below
    // skip glyph / underline / cursor emission for any cell whose
    // (row, col) falls inside.
    const SEARCH_BAR_COLS: u16 = 40;
    const SEARCH_LIST_MAX_ROWS: u16 = 10;
    let overlay_mask: Option<(u16, u16, u16, u16)> = view.search_overlay.as_ref()
        .and_then(|ov| {
            let cols = grid.cols();
            if cols < SEARCH_BAR_COLS + 2 { return None; }
            // Mirror the F1+11 pixel-mode overlay geometry: the panel
            // floats ~½ cell down from the grid top, occupies query
            // (1 row) + divider (½ row) + list rows + 2× inner
            // padding.  Round generously upward so grid glyphs under
            // the panel are masked.
            let list_rows = (ov.hits.len() as u16).min(SEARCH_LIST_MAX_ROWS);
            // 1 (margin) + 1 (query) + 1 (divider region) + list + 1 (bottom pad)
            let covered_rows = 1 + 1 + 1 + list_rows + 1;
            let col_start = cols - SEARCH_BAR_COLS - 1;
            let col_end_inclusive = cols - 2;
            let row_start: u16 = 0;
            let row_end_exclusive = covered_rows.min(grid.rows());
            Some((row_start, row_end_exclusive, col_start, col_end_inclusive))
        });
    // F1+13 — fast-path: when the overlay is closed (the common case),
    // bail out without touching `overlay_mask`'s scrutinee on every
    // per-cell call.  Keeps `under_overlay` a tight inline check on
    // the per-row / per-cell hot path.
    // The same masking, for the cells an IME composition covers.
    // Computed here because the per-cell loops below run before the
    // preedit is drawn, and they are what must leave those cells
    // empty; the drawing reads the same placements.
    let preedit_cells: Vec<(u16, u16, u16, &str)> = if view.view_offset == 0
        && view.focused
        && window_focused
        && !view.ime_preedit.is_empty()
    {
        preedit_placements(view.ime_preedit, grid.cursor(), grid.cols(), grid.rows())
    } else {
        Vec::new()
    };
    // Same fast path as `overlay_active`: nothing composing is the
    // common case, and this is a per-cell call.

    // Scan once up front so the per-row glyph loop can override fg
    // for cells inside a link span (paint the text the same cyan as
    // the underline, the standard "this is clickable" cue) and the
    // underline pass below can reuse the same list.
    // An agent TUI wraps its own lines, so tell the link scanner to
    // merge the hanging-indent continuation into one logical URL /
    // path token.  Inert on ordinary panes.  See `SessionView::
    // agent_tui` for why this is no longer inferred from the badge.
    let link_opts = marspot_term::grid_links::ScanOpts {
        cc_mode: view.agent_tui,
        cwd: (!view.cwd.is_empty()).then_some(view.cwd),
    };
    // The oracle is what keeps this call off the filesystem: on the
    // render thread a single `lstat` under a network mount or the
    // `auto_home` autofs map has been measured at six seconds.  See
    // `marspot::link_probe`.
    let links = marspot_term::grid_links::scan_visible_links_with(
        grid,
        view.view_offset,
        link_opts,
        crate::link_probe::oracle(),
    );

    let g = PaneGeom {
        view,
        grid,
        cols,
        cell_w,
        cell_h,
        ascent,
        atlas_w,
        atlas_h,
        color_atlas_w,
        color_atlas_h,
        inner_x,
        inner_y,
        window_focused,
        overlay_mask,
        preedit_cells,
        links,
    };
    let mut o = PaneOut { font, atlas, color_atlas, cells, glyphs, color_glyphs };
    paint_rows(&g, &mut o);
    paint_selection(&g, &mut o);
    paint_link_underlines(&g, &mut o);
    paint_cursor_glyph(&g, &mut o);
    paint_cursor(&g, &mut o);
    paint_preedit(&g, &mut o);

    // No darken overlay.  No FOCUS_OUTLINE blue frame.  The focus
    // affordance is the BG_FOCUSED tint applied to the cell rect at
    // the top of this fn, plus the solid-vs-hollow cursor — both
    // already done above.  iTerm2 reads exactly this way: no
    // chrome, no border, just a quiet BG lift on the active pane.
    let _ = gutter; // pane-internal layout doesn't use it any more

    // F1+11 — true pixel-mode UI overlay.  Drawn via the new
    // `ui_rect_pipeline`: a SDF-based rounded rectangle with
    // anti-aliased corners, optional stroke, and a soft drop shadow
    // — NOT cell-grid characters.  Pixel-precise positioning, freed
    // from the box-drawing approximation.  Text inside still goes
    // through the cell glyph atlas (proportional UI font is Phase 2),
    // but everything BEHIND the text — panel, borders, focused-row
    // selection — is real GPU-side vector chrome.
    //
    // Layout:
    //   ┌── panel(rounded rect + shadow + 1px stroke)
    //   │  query row : query text + caret
    //   │  divider line (thin rounded rect, dim color)
    //   │  list rows : focused row is a separate rounded rect
    //   │              behind the snippet text
    //   └──
    //
    // Palette: Darcula-ish dark gray-blue panel, JetBrains-style
    // indigo selection (NOT the yellow grid-side HIGHLIGHT_BG —
    // those have distinct semantic meanings and must not collide).
    // F3+1.8 — search overlay rendering moved to
    // `ui::components::search_overlay::paint_search_overlay`, called
    // from `build_instances` post-pane-loop into the overlay
    // scratches.  push_session no longer touches it; the per-pane
    // instance cache stays purely grid content.
}

/// The grid, row by row: run-length backgrounds, search-hit highlights,
/// glyphs and SGR underlines.
pub(crate) fn paint_rows(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { view, grid, cols, cell_w, cell_h, ascent, atlas_w, atlas_h, color_atlas_w, color_atlas_h, inner_x, inner_y, window_focused, .. } = *g;
    let font = &mut *o.font;
    let atlas = &mut *o.atlas;
    let color_atlas = &mut *o.color_atlas;
    let cells = &mut *o.cells;
    let glyphs = &mut *o.glyphs;
    let color_glyphs = &mut *o.color_glyphs;
    // One row's cluster text, refilled per row and reused across them.
    // A cell that has scrolled into history holds the base codepoint,
    // so what the cluster said is keyed by where it was -- one lookup
    // for the row, not one per cell per frame.
    let mut row_clusters: Vec<(u16, String)> = Vec::new();
    for r in 0..grid.rows() {
        let row_y = inner_y + (r as f32) * cell_h;
        let _baseline_y = row_y + ascent;

        // Run-length BG fills (skip default BG; it inherits the rect fill).
        let mut c = 0usize;
        while c < cols {
            let cell = grid.cell_at_view(view.view_offset, c as u16, r);
            let bg = resolve_attrs(cell.attrs).1;
            if bg == BG {
                c += 1;
                continue;
            }
            let start = c;
            c += 1;
            while c < cols {
                let cur = grid.cell_at_view(view.view_offset, c as u16, r);
                if resolve_attrs(cur.attrs).1 != bg {
                    break;
                }
                c += 1;
            }
            cells.push(CellInstance {
                origin: [inner_x + start as f32 * cell_w, row_y],
                size: [(c - start) as f32 * cell_w, cell_h],
                color: rgba8_of_f32([bg.0 as f32, bg.1 as f32, bg.2 as f32, 1.0]),
            });
        }

        // C4 — search-hit highlight BG.  Drawn AFTER cell BG so it
        // wins z-order, BEFORE glyphs so they paint on top with
        // their original FG (the "highlight yellow BG, original fg
        // preserved" rule from §6.8).  Bounded by grid cols — a
        // span fed with `col_end_inclusive >= cols` is clamped to
        // the right edge.  Per-row loop: O(spans) work scoped to
        // rows where the highlight lives; renderer p99 delta on a
        // typical single-row span = one extra `CellInstance` push.
        for span in view.highlight_spans {
            if span.view_row != r {
                continue;
            }
            let cols_u16 = cols as u16;
            if span.col_start >= cols_u16 {
                continue;
            }
            let col_end = span.col_end_inclusive.min(cols_u16 - 1);
            if span.col_start > col_end {
                continue;
            }
            let n_cols = (col_end + 1 - span.col_start) as f32;
            cells.push(CellInstance {
                origin: [inner_x + span.col_start as f32 * cell_w, row_y],
                size: [n_cols * cell_w, cell_h],
                color: rgba8_of_f32([HIGHLIGHT_BG.0, HIGHLIGHT_BG.1, HIGHLIGHT_BG.2, 1.0]),
            });
        }

        // Glyphs.  Skip the cursor cell when the cursor is solid —
        // we re-emit it after with BG colour so the glyph reads
        // inverted on the white cursor block (mirrors render.rs).
        grid.row_clusters_at_view(view.view_offset, r, &mut row_clusters);
        let cursor = grid.cursor();
        let solid_cursor = view.view_offset == 0
            && view.cursor_visible
            && view.focused
            && window_focused;
        for c in 0..cols {
            if solid_cursor && cursor == (c as u16, r) {
                continue;
            }
            // F1++ — skip grid glyphs covered by the search overlay
            // chrome (BG / FG run in separate passes so without this
            // mask, the underlying terminal text bleeds through the
            // opaque overlay).
            if g.under_overlay(r, c as u16) || g.under_preedit(r, c as u16) {
                continue;
            }
            let cell = grid.cell_at_view(view.view_offset, c as u16, r);
            if cell.ch == ' ' || cell.ch == '\0' {
                continue;
            }
            let metrics = SlotMetrics {
                cell_w: cell_w.round() as u32,
                cell_h: cell_h.round() as u32,
                baseline_from_top: ascent.round() as u32,
            };
            // A cell holding more than one codepoint is drawn as the
            // whole cluster; everything else takes the path it always
            // did, byte for byte.
            let cluster = grid
                .cluster_text(&cell)
                .or_else(|| marspot_term::grid::cluster_in_row(&row_clusters, c as u16))
                .map(str::to_string);
            let (entry, is_color) = match cluster {
                Some(text) => match resolve_cluster_glyph(
                    atlas,
                    font,
                    &text,
                    cell.attrs.bold,
                    cell.attrs.italic,
                    metrics,
                ) {
                    Some(e) => (e, false),
                    None => continue,
                },
                None => match resolve_cell_glyph_routed(
                    atlas,
                    color_atlas,
                    font,
                    cell.ch,
                    cell.attrs.bold,
                    cell.attrs.italic,
                    metrics,
                ) {
                    Some(e) => e,
                    None => continue,
                },
            };
            let fg = if g.in_link(r, c as u16) {
                (
                    LINK_UNDERLINE_FG.0 as f64,
                    LINK_UNDERLINE_FG.1 as f64,
                    LINK_UNDERLINE_FG.2 as f64,
                )
            } else {
                resolve_attrs(cell.attrs).0
            };
            // Phase 1.1 bearing formula — see `AtlasEntry::quad`.
            // pen_x = cell origin; baseline_y = slot top + ascent (use
            // the same integer `baseline_from_top` that drove the
            // rasteriser so origin.y reduces to row_y.round() for
            // Phase 1.0 entries — bit-equivalent to the pre-Phase-1.1
            // cell-aligned formula).
            let pen_x = (inner_x + c as f32 * cell_w).round();
            let baseline_y = row_y.round() + metrics.baseline_from_top as f32;
            let (origin, size) = entry.quad(pen_x, baseline_y);
            // Colour glyphs (emoji) go to the colour buffer + atlas; the
            // colour shader samples their real pixels and ignores `color`
            // (except its alpha, used for pane-dim).  Mono glyphs are
            // tinted by `fg` as before.
            let (aw, ah, sink) = if is_color {
                (color_atlas_w, color_atlas_h, &mut *color_glyphs)
            } else {
                (atlas_w, atlas_h, &mut *glyphs)
            };
            sink.push(GlyphInstance {
                origin,
                size,
                uv0: [entry.u0 as f32 / aw, entry.v0 as f32 / ah],
                uv1: [entry.u1 as f32 / aw, entry.v1 as f32 / ah],
                color: rgba8_of_f32([fg.0 as f32, fg.1 as f32, fg.2 as f32, 1.0]),
            });
        }

        // Underline pass — BG-pass coloured rectangles below the
        // baseline.  Run-length over consecutive same-fg underlined
        // cells.  Geometry matches render.rs (`(cell_h - ascent) * 0.55`,
        // `(cell_h * 0.06).max(1.0)`).
        let underline_y = row_y + cell_h - (cell_h - ascent) * 0.45;
        let underline_h = (cell_h * 0.06).max(1.0);
        let mut u = 0usize;
        while u < cols {
            let cell = grid.cell_at_view(view.view_offset, u as u16, r);
            if !cell.attrs.underline {
                u += 1;
                continue;
            }
            let fg = resolve_attrs(cell.attrs).0;
            let start = u;
            u += 1;
            while u < cols {
                let cur = grid.cell_at_view(view.view_offset, u as u16, r);
                if !cur.attrs.underline || resolve_attrs(cur.attrs).0 != fg {
                    break;
                }
                u += 1;
            }
            cells.push(CellInstance {
                origin: [inner_x + start as f32 * cell_w, underline_y],
                size: [(u - start) as f32 * cell_w, underline_h],
                color: rgba8_of_f32([fg.0 as f32, fg.1 as f32, fg.2 as f32, 1.0]),
            });
        }
    }
}
