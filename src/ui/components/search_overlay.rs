//! Per-pane search overlay: floating popover with query row + counter +
//! Aa toggle + × close + scrollable hit list.  Built on top of `View`
//! so it shares the always-on-top + opaque-by-default invariants.
//!
//! Originally lived as ~170 lines inline inside `render_metal::push_session`
//! — same shape as the Process Monitor before F3+1.6, with the same
//! latent "background leaks grid glyphs" failure mode.  Migrated here
//! in F3+1.8 so it routes through overlay scratches automatically.

use marspot_term::layout::Rect;
use marspot_term::render::SearchOverlayView;
use crate::ui::core::{View, ViewStyle, ViewPainter, Backdrop};

/// What the renderer needs to position + paint one pane's search
/// overlay.  Caller (build_instances) gathers per-pane fields from
/// the layout cell + SessionView once they're known and hands them
/// over.  Lifetimes inherit from `SearchOverlayView`'s borrowed
/// `hits` / `query` strings — the renderer never stores any of this
/// across frames.
pub struct SearchOverlayParams<'a> {
    pub overlay: &'a SearchOverlayView,
    /// Pane's inner content origin (after pane padding + cell title).
    pub inner_x: f32,
    pub inner_y: f32,
    /// Grid dimensions of the pane (cell count, not pixels).
    pub grid_cols: u16,
    pub grid_rows: u16,
}

// Palette: Darcula-ish dark gray-blue panel, JetBrains-style indigo
// selection.  Distinct from grid-side HIGHLIGHT_BG so semantic
// meanings don't collide.
const OVERLAY_BG:          [f32; 4] = [0.13, 0.14, 0.17, 1.0];
const OVERLAY_BORDER:      [f32; 4] = [0.30, 0.32, 0.38, 1.0];
const OVERLAY_TEXT:        [f32; 4] = [0.95, 0.96, 0.97, 1.0];
const OVERLAY_DIM:         [f32; 4] = [0.60, 0.63, 0.70, 1.0];
const OVERLAY_ACCENT:      [f32; 4] = [0.40, 0.62, 1.00, 1.0];
const OVERLAY_FOCUSED_BG:  [f32; 4] = [0.18, 0.28, 0.48, 1.0];
const OVERLAY_DIVIDER:     [f32; 4] = [0.22, 0.24, 0.28, 1.0];
const OVERLAY_CARET:       [f32; 4] = [0.40, 0.62, 1.00, 0.95];
const PANEL_RADIUS_PX: f32 = 10.0;
const ROW_RADIUS_PX: f32 = 5.0;
const SHADOW_BLUR_PX: f32 = 18.0;
const SHADOW_ALPHA: f32 = 0.45;

// Mirrors the consts that lived inline in `render_metal::push_session`
// before F3+1.8.  If a future feature wants a wider bar, the source of
// truth moves here.
pub const OVERLAY_COLS: u16 = 40;
pub const LIST_MAX_ROWS: u16 = 10;

/// Paint one pane's search overlay (or no-op when the pane doesn't
/// have one).  Routes through `ViewPainter` → overlay scratches, so
/// the bar always sits above the pane's grid glyphs by construction.
pub fn paint_search_overlay(p: &mut ViewPainter, params: SearchOverlayParams<'_>) {
    let overlay = params.overlay;
    if params.grid_cols < OVERLAY_COLS + 2 {
        return;
    }
    let cell_w = p.cell_w;
    let cell_h = p.cell_h;
    let ascent = p.ascent;
    let inner_x = params.inner_x;
    let inner_y = params.inner_y;
    let grid_bottom = inner_y + (params.grid_rows as f32) * cell_h;

    let n_list = (overlay.hits.len() as u16).min(LIST_MAX_ROWS);
    let has_list = n_list > 0;
    let chrome_rows: f32 = 1.0;
    let divider_rows: f32 = if has_list { 0.4 } else { 0.0 };
    let list_rows: f32 = if has_list { n_list as f32 } else { 0.0 };
    let total_rows_f = chrome_rows + divider_rows + list_rows;
    let panel_inner_pad = (cell_h * 0.4).max(6.0);
    let panel_h_uncapped = (total_rows_f * cell_h).round() + 2.0 * panel_inner_pad;
    let bar_left_col = params.grid_cols - OVERLAY_COLS - 1;
    let bar_x = inner_x + (bar_left_col as f32) * cell_w;
    let panel_y = inner_y + (cell_h * 0.5).round();
    let panel_w = (OVERLAY_COLS as f32) * cell_w;
    let panel_h = panel_h_uncapped.min((grid_bottom - panel_y).max(0.0));

    let view = View {
        rect: Rect {
            x: bar_x as f64,
            y_top: panel_y as f64,
            w: panel_w as f64,
            h: panel_h as f64,
        },
        style: ViewStyle {
            bg: OVERLAY_BG,
            border_color: OVERLAY_BORDER,
            border_width: 1.0,
            corner_radius: PANEL_RADIUS_PX,
            shadow_blur: SHADOW_BLUR_PX,
            shadow_alpha: SHADOW_ALPHA,
            backdrop: Backdrop::None,
        },
    };

    view.paint(p, |p| {
        let inner_left = bar_x + panel_inner_pad;
        let inner_top = panel_y + panel_inner_pad;

        // Query row text.
        let query_baseline = inner_top + ascent;
        let query_x = inner_left;
        let query_max_chars = (OVERLAY_COLS - 12) as usize;
        let query_display: String =
            overlay.query.chars().take(query_max_chars).collect();
        if !query_display.is_empty() {
            p.text(query_x, query_baseline, &query_display, OVERLAY_TEXT);
        }
        // Caret: thin accent vertical at query_cursor column.
        let caret_col = (overlay.query_cursor as usize).min(query_max_chars) as f32;
        let caret_x = query_x + caret_col * cell_w;
        p.fill_rect(
            Rect {
                x: caret_x as f64,
                y_top: (inner_top + 2.0) as f64,
                w: 2.0,
                h: (cell_h - 4.0) as f64,
            },
            OVERLAY_CARET,
        );
        // × close hint.
        let close_x = bar_x + panel_w - panel_inner_pad - cell_w;
        p.text(close_x, query_baseline, "×", OVERLAY_DIM);
        // Aa toggle (accent when case-sensitive).
        let aa_x = close_x - 3.0 * cell_w;
        let aa_color = if overlay.case_sensitive { OVERLAY_ACCENT } else { OVERLAY_DIM };
        p.text(aa_x, query_baseline, "Aa", aa_color);
        // Counter "4/64".
        if let Some((c, t)) = overlay.counter {
            let counter_text = format!("{c}/{t}");
            let counter_w_chars = counter_text.chars().count() as f32;
            let counter_x = aa_x - (counter_w_chars + 1.0) * cell_w;
            p.text(counter_x, query_baseline, &counter_text, OVERLAY_DIM);
        }

        // Divider.
        let divider_y = inner_top + cell_h + (panel_inner_pad * 0.5).round();
        if has_list {
            p.fill_rounded_rect(
                Rect {
                    x: inner_left as f64,
                    y_top: divider_y as f64,
                    w: (panel_w - 2.0 * panel_inner_pad) as f64,
                    h: 1.0,
                },
                OVERLAY_DIVIDER,
                0.5,
                ([0.0, 0.0, 0.0, 0.0], 0.0),
            );
        }

        // List rows.
        let list_top = divider_y + (panel_inner_pad * 0.5).round();
        let max_visible = ((grid_bottom - list_top) / cell_h).floor() as u16;
        let visible = n_list.min(max_visible).min(LIST_MAX_ROWS);
        for i in 0..visible {
            let h = &overlay.hits[i as usize];
            let row_y = list_top + (i as f32) * cell_h;
            let row_baseline = row_y + ascent;
            if h.is_focused {
                let inset = 4.0;
                p.fill_rounded_rect(
                    Rect {
                        x: (bar_x + inset) as f64,
                        y_top: row_y as f64,
                        w: (panel_w - 2.0 * inset) as f64,
                        h: cell_h as f64,
                    },
                    OVERLAY_FOCUSED_BG,
                    ROW_RADIUS_PX,
                    ([0.0, 0.0, 0.0, 0.0], 0.0),
                );
            }
            let inner_w_chars = (OVERLAY_COLS - 2) as usize;
            let snip: String = h.snippet.chars().take(inner_w_chars).collect();
            let snip_color = if h.is_focused { OVERLAY_TEXT } else { OVERLAY_DIM };
            p.text(inner_left, row_baseline, &snip, snip_color);
        }
    });
}
