//! What is drawn over a pane's grid: its title strip, the selection,
//! link underlines, the cursor and an IME composition.

use crate::font_cache::{FontCache, BG};
use crate::frame_build::glyph_resolve::{resolve_cell_glyph, resolve_cluster_glyph};
use crate::frame_build::palette::*;
use crate::frame_build::text_run::push_text_run;
use crate::glyph_atlas::{GlyphAtlas, SlotMetrics};
use crate::layout::CellRect;
use crate::render::SessionView;
use crate::render_metal::{rgba8_of_f32, CellInstance, GlyphInstance};
use crate::frame_build::session::{PaneGeom, PaneOut};

/// The pane's title strip: its seam, its label, the plugin badge and the
/// refresh affordance.  Drawn at the unrounded cell metrics, as it always was.
pub(crate) fn paint_title_strip(
    rect: &CellRect,
    view: &SessionView,
    pane_focused: bool,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    gutter: f32,
    padding: f32,
    title_h: f32,
) {
    // Title strip — band at the top of the cell hosting the
    // session label.  BG sits flush with the cell BG; a single
    // SEAM hairline along the bottom of the strip separates it
    // from the terminal content area below.  Glyphs in the strip
    // use SIDEBAR_TEXT_FG (dim greyish-blue) so the title reads
    // as quiet metadata, not as content competing with the
    // terminal text.
    if title_h > 0.0 && !view.title.is_empty() {
        let strip_bottom = rect.y_top as f32 + title_h;
        // Bottom seam (1 phys-px line just inside the strip's
        // bottom edge — sits exactly where padding starts).
        cells.push(CellInstance {
            origin: [rect.x as f32, strip_bottom - gutter.max(1.0)],
            size: [rect.w as f32, gutter.max(1.0)],
            color: rgba8_of_f32([SEAM.0, SEAM.1, SEAM.2, 1.0]),
        });
        // Title text — same monospace metrics as the terminal
        // body; vertically centred in the strip, left-aligned
        // with the same padding the terminal uses.
        let label_x = rect.x as f32 + padding;
        let label_baseline_y = rect.y_top as f32
            + (title_h - cell_h) * 0.5
            + ascent;
        push_text_run(
            view.title,
            label_x,
            label_baseline_y,
            [SIDEBAR_TEXT_FG.0, SIDEBAR_TEXT_FG.1, SIDEBAR_TEXT_FG.2, 1.0],
            cell_w,
            cell_h,
            ascent,
            atlas_w,
            atlas_h,
            font,
            atlas,
            glyphs,
        );
        // Plugin badge (claudecode etc.): right-edge decoration in
        // claudecode coral.  Whatever the badge text is BEFORE the
        // first ' ' is treated as the *clickable prefix* (today:
        // "P1" / "P2" / "P3") and gets a hairline underline so the
        // user reads it as actionable; whatever follows is rendered
        // plain (the sessionId).  The plugin (L1) — not the renderer
        // — owns the meaning of the prefix; this just decorates it.
        if !view.right_badge.is_empty() {
            let badge_chars = view.right_badge.chars().count() as f32;
            // Right anchor: leave room for the refresh affordance
            // (one cell + a half-cell gap) when both are present,
            // otherwise hug the right edge with a single padding.
            let reserved = if view.update_pending && pane_focused {
                cell_w * 1.5
            } else {
                0.0
            };
            let badge_x = rect.x as f32 + rect.w as f32
                - padding
                - reserved
                - badge_chars * cell_w;
            if badge_x > label_x {
                push_text_run(
                    view.right_badge,
                    badge_x,
                    label_baseline_y,
                    [PLUGIN_BADGE_FG.0, PLUGIN_BADGE_FG.1, PLUGIN_BADGE_FG.2, 1.0],
                    cell_w,
                    cell_h,
                    ascent,
                    atlas_w,
                    atlas_h,
                    font,
                    atlas,
                    glyphs,
                );
                // Hairline underline under the prefix (text before the
                // first space).  Width = prefix_chars × cell_w; sits
                // 1 phys-px below the baseline so it doesn't clip
                // descenders that won't appear in `P<digit>` anyway.
                let prefix_chars = view
                    .right_badge
                    .split(' ')
                    .next()
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                if prefix_chars > 0 {
                    let underline_y = label_baseline_y + gutter.max(1.0);
                    cells.push(CellInstance {
                        origin: [badge_x, underline_y],
                        size: [prefix_chars as f32 * cell_w, gutter.max(1.0)],
                        color: rgba8_of_f32([
                            PLUGIN_BADGE_FG.0,
                            PLUGIN_BADGE_FG.1,
                            PLUGIN_BADGE_FG.2,
                            1.0,
                        ]),
                    });
                }
            }
        }
        // Deferred-update affordance (target #4 step 5b): a refresh glyph
        // at the right edge of the *focused* pane's title strip when a
        // silent swap is staged for it.  Clicking it (hit-tested via
        // `Layout::hit_test_cell_refresh`) triggers the swap.  Brighter
        // than the dim title text so it reads as an actionable control.
        if view.update_pending && pane_focused {
            let icon_x = rect.x as f32 + rect.w as f32 - padding - cell_w;
            push_text_run(
                "\u{27F3}", // ⟳ CLOCKWISE GONG WITH CIRCLE ARROW
                icon_x,
                label_baseline_y,
                [REFRESH_ICON_FG.0, REFRESH_ICON_FG.1, REFRESH_ICON_FG.2, 1.0],
                cell_w,
                cell_h,
                ascent,
                atlas_w,
                atlas_h,
                font,
                atlas,
                glyphs,
            );
        }
    }
}

/// The selection band, over the cell backgrounds so it shows on any of them.
pub(crate) fn paint_selection(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { view, grid, cell_w, cell_h, inner_x, inner_y, .. } = *g;
    let cells = &mut *o.cells;
    // Selection BG — paint a single quad per selected row, AFTER
    // the per-cell run-length BG fills.  Order matters: the BG
    // pass writes opaque pixels and the last instance wins, so a
    // selection drawn before the per-cell BG would be hidden on
    // any cell carrying an ANSI-coloured BG (red `git diff -`,
    // green `git diff +`, etc.).  Drawing it here paints over
    // the cell colours so the highlight is always visible while
    // active.
    if let Some(sel) = view.selection {
        let (anchor, focus) = (sel.anchor, sel.focus);
        let max_row = grid.rows().saturating_sub(1);
        let max_col = grid.cols().saturating_sub(1);
        if sel.blockwise {
            // Rectangle: each row from min..=max col, independent
            // of row position.  Lets the user carve out a column
            // from multi-column output (ls, top) without dragging
            // the column-aligned padding along.
            let r_lo = anchor.1.min(focus.1).min(max_row);
            let r_hi = anchor.1.max(focus.1).min(max_row);
            let c_lo = anchor.0.min(focus.0).min(max_col);
            let c_hi = anchor.0.max(focus.0).min(max_col);
            if c_hi >= c_lo {
                let w = (c_hi - c_lo + 1) as f32 * cell_w;
                for r in r_lo..=r_hi {
                    cells.push(CellInstance {
                        origin: [
                            inner_x + c_lo as f32 * cell_w,
                            inner_y + r as f32 * cell_h,
                        ],
                        size: [w, cell_h],
                        color: rgba8_of_f32([SELECTION_BG.0, SELECTION_BG.1, SELECTION_BG.2, 1.0]),
                    });
                }
            }
        } else {
            // Row-band: top row from anchor.col to end, middle rows
            // full width, bottom row from start to focus.col.
            let (start, end) = if (anchor.1, anchor.0) <= (focus.1, focus.0) {
                (anchor, focus)
            } else {
                (focus, anchor)
            };
            let (s_col, s_row) = start;
            let (e_col, e_row) = end;
            let s_row = s_row.min(max_row);
            let e_row = e_row.min(max_row);
            for r in s_row..=e_row {
                let col_lo = if r == s_row { s_col } else { 0 };
                let col_hi = if r == e_row { e_col } else { max_col };
                let col_lo = col_lo.min(max_col);
                let col_hi = col_hi.min(max_col);
                if col_hi < col_lo {
                    continue;
                }
                let x = inner_x + col_lo as f32 * cell_w;
                let y = inner_y + r as f32 * cell_h;
                let w = (col_hi - col_lo + 1) as f32 * cell_w;
                cells.push(CellInstance {
                    origin: [x, y],
                    size: [w, cell_h],
                    color: rgba8_of_f32([SELECTION_BG.0, SELECTION_BG.1, SELECTION_BG.2, 1.0]),
                });
            }
        }
    }
}

/// One underline per detected link, over the selection.
pub(crate) fn paint_link_underlines(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { cell_w, cell_h, ascent, inner_x, inner_y, .. } = *g;
    let cells = &mut *o.cells;
    // Auto-link underline — reuse `links` from the up-front scan.
    // Runs AFTER selection so the link hint stays visible when the
    // user drags over a link (the selection blue tints it but the
    // underline sits on top).  Same geometry as the SGR-underline
    // pass above, so a link sitting on already-underlined text just
    // paints the link colour over the same row.
    {
        for link in &g.links {
            let row_y = inner_y + (link.row as f32) * cell_h;
            let underline_y = row_y + cell_h - (cell_h - ascent) * 0.45;
            let underline_h = (cell_h * 0.06).max(1.0);
            let cols_in_span = link.col_end.saturating_sub(link.col_start) + 1;
            cells.push(CellInstance {
                origin: [
                    inner_x + link.col_start as f32 * cell_w,
                    underline_y,
                ],
                size: [cols_in_span as f32 * cell_w, underline_h],
                color: rgba8_of_f32([
                    LINK_UNDERLINE_FG.0,
                    LINK_UNDERLINE_FG.1,
                    LINK_UNDERLINE_FG.2,
                    1.0,
                ]),
            });
        }
    }
}

/// The glyph under a solid cursor, redrawn in the background colour.
pub(crate) fn paint_cursor_glyph(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { view, grid, cell_w, cell_h, ascent, atlas_w, atlas_h, inner_x, inner_y, window_focused, .. } = *g;
    let font = &mut *o.font;
    let atlas = &mut *o.atlas;
    let glyphs = &mut *o.glyphs;
    // Cursor cell glyph in BG colour (re-rendered atop the white
    // cursor block) — only when the cursor is solid.  Hollow cursor
    // doesn't cover the glyph, so the normal FG glyph above suffices.
    if view.view_offset == 0
        && view.cursor_visible
        && view.focused
        && window_focused
    {
        let (col, row) = grid.cursor();
        // F1++ — skip cursor glyph re-emit when the cursor lands
        // under the overlay (would bleed through the BG pass).
        if !g.under_overlay(row, col) && !g.under_preedit(row, col) {
        let cell = grid.cell_at_view(0, col, row);
        if cell.ch != ' ' && cell.ch != '\0' {
            let metrics = SlotMetrics {
                cell_w: cell_w.round() as u32,
                cell_h: cell_h.round() as u32,
                baseline_from_top: ascent.round() as u32,
            };
            // Same split as the main pass: a cluster under the cursor
            // is drawn whole, or its index would be drawn as a glyph.
            let mut cursor_row_clusters = Vec::new();
            grid.row_clusters_at_view(0, row, &mut cursor_row_clusters);
            let cursor_glyph = match grid
                .cluster_text(&cell)
                .or_else(|| marspot_term::grid::cluster_in_row(&cursor_row_clusters, col))
            {
                Some(text) => {
                    let owned = text.to_string();
                    resolve_cluster_glyph(
                        atlas, font, &owned, cell.attrs.bold, cell.attrs.italic, metrics,
                    )
                }
                None => resolve_cell_glyph(
                    atlas,
                    font,
                    cell.ch,
                    cell.attrs.bold,
                    cell.attrs.italic,
                    metrics,
                ),
            };
            if let Some(entry) = cursor_glyph {
                // Phase 1.1 bearing formula (cursor BG re-emit).
                let pen_x = (inner_x + col as f32 * cell_w).round();
                let dest_y = (inner_y + (row as f32) * cell_h).round();
                let baseline_y = dest_y + metrics.baseline_from_top as f32;
                let (origin, size) = entry.quad(pen_x, baseline_y);
                glyphs.push(GlyphInstance {
                    origin,
                    size,
                    uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                    uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                    color: rgba8_of_f32([BG.0 as f32, BG.1 as f32, BG.2 as f32, 1.0]),
                });
            }
        }
        }
    }
}

/// The cursor block, solid when the pane and window are focused, else hollow.
pub(crate) fn paint_cursor(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { view, grid, cell_w, cell_h, inner_x, inner_y, window_focused, .. } = *g;
    let cells = &mut *o.cells;
    // Cursor (live view + DECTCEM on).
    if view.view_offset == 0 && view.cursor_visible {
        let (col, row) = grid.cursor();
        let cx = inner_x + col as f32 * cell_w;
        let cy = inner_y + row as f32 * cell_h;
        let solid = view.focused && window_focused;
        let color = [CURSOR_FG.0, CURSOR_FG.1, CURSOR_FG.2, 1.0];
        if solid {
            let color = rgba8_of_f32(color);
            cells.push(CellInstance {
                origin: [cx, cy],
                size: [cell_w, cell_h],
                color,
            });
        } else {
            // Hollow: 4 stroke quads.  Stroke width tracks render.rs.
            let stroke = (cell_h * 0.07).max(1.0);
            let color = rgba8_of_f32(color);
            cells.push(CellInstance { origin: [cx, cy], size: [cell_w, stroke], color });
            cells.push(CellInstance { origin: [cx, cy + cell_h - stroke], size: [cell_w, stroke], color });
            cells.push(CellInstance { origin: [cx, cy], size: [stroke, cell_h], color });
            cells.push(CellInstance { origin: [cx + cell_w - stroke, cy], size: [stroke, cell_h], color });
        }
    }
}

/// The IME composition, over the cells the loops above left empty for it.
pub(crate) fn paint_preedit(g: &PaneGeom, o: &mut PaneOut) {
    let PaneGeom { cell_w, cell_h, ascent, atlas_w, atlas_h, inner_x, inner_y, .. } = *g;
    let font = &mut *o.font;
    let atlas = &mut *o.atlas;
    let cells = &mut *o.cells;
    let glyphs = &mut *o.glyphs;
    // IME preedit overlay — paint the in-flight composition at the
    // cursor position so the user sees pinyin / hiragana before the
    // IME commits.  Only when the pane is focused, live, and the host
    // window has focus; otherwise the cursor anchor isn't visible /
    // interactive.
    //
    // Where each cluster goes was decided before the loops above ran
    // (`preedit_cells`), and those loops left the covered cells empty
    // — without that, the BG quad here hides the cells' BACKGROUND
    // and the FG pass then draws the terminal's own glyphs back on
    // top of the composition.
    if !g.preedit_cells.is_empty() {
        let metrics = SlotMetrics {
            cell_w: cell_w.round() as u32,
            cell_h: cell_h.round() as u32,
            baseline_from_top: ascent.round() as u32,
        };
        for &(r, c, n_cells, cluster) in &g.preedit_cells {
            let dest_x = (inner_x + c as f32 * cell_w).round();
            let dest_y = (inner_y + r as f32 * cell_h).round();
            let slot_w = n_cells as f32 * cell_w;
            // BG quad — opaque, spanning the whole cluster.
            cells.push(CellInstance {
                origin: [dest_x, dest_y],
                size: [slot_w, cell_h],
                color: rgba8_of_f32([IME_PREEDIT_BG.0, IME_PREEDIT_BG.1, IME_PREEDIT_BG.2, 1.0]),
            });
            // Glyph — same atlas path the cell-render uses, so the
            // preedit text is rendered at the EXACT same px size as
            // a normal cell.  A grapheme cluster's lead codepoint
            // drives the atlas lookup (the rest are combining marks
            // / ZWJ joiners we don't render inline yet — acceptable
            // first-cut, the candidate window is the source of truth
            // anyway).
            if let Some(lead) = cluster.chars().next()
                && let Some(entry) = resolve_cell_glyph(atlas, font, lead, false, false, metrics) {
                    // Phase 1.1 bearing formula (IME preedit).
                    let baseline_y = dest_y + metrics.baseline_from_top as f32;
                    let (origin, size) = entry.quad(dest_x, baseline_y);
                    glyphs.push(GlyphInstance {
                        origin,
                        size,
                        uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                        uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                        color: rgba8_of_f32([IME_PREEDIT_FG.0, IME_PREEDIT_FG.1, IME_PREEDIT_FG.2, 1.0]),
                    });
                }
            // Underline — 2× the old hairline so it actually reads
            // as "this is provisional text" against the BG quad.
            // Hairline (cell_h * 0.06) was invisible at small font
            // sizes (user feedback 2026-06-15 "好小好小").
            let underline_h = (cell_h * 0.12).max(2.0).round();
            cells.push(CellInstance {
                origin: [dest_x, dest_y + cell_h - underline_h],
                size: [slot_w, underline_h],
                color: rgba8_of_f32([IME_PREEDIT_FG.0, IME_PREEDIT_FG.1, IME_PREEDIT_FG.2, 1.0]),
            });
        }
    }
}
