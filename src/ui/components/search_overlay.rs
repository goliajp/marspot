//! Per-pane search overlay — composed from primitive UI kit pieces:
//!
//!   - `core::View` (always-on-top + opaque BG via overlay scratches)
//!   - `components::Panel` (View wrapper with content padding)
//!   - `components::TextInput` (query text + caret)
//!   - `components::ListView` (results list with focused-row highlight)
//!
//! The free-standing icons (× close, Aa toggle, M/N counter) are still
//! rendered inline because they're tiny widgets only this scene uses
//! — pure colocate-per-React.  If a second feature wants them, lift
//! into their own components.

use marspot_term::layout::Rect;
use marspot_term::render::SearchOverlayView;
use crate::ui::core::{ViewPainter, ViewStyle, Backdrop};
use super::{
    Panel, TextInput, TextInputStyle, ListView, ListRow, ListViewStyle,
    Button, ButtonStyle, IconSpec, IconPosition,
};

pub struct SearchOverlayParams<'a> {
    pub overlay: &'a SearchOverlayView,
    pub inner_x: f32,
    pub inner_y: f32,
    pub grid_cols: u16,
    pub grid_rows: u16,
}

// Palette
const PANEL_BG:        [f32; 4] = [0.13, 0.14, 0.17, 1.0];
const PANEL_BORDER:    [f32; 4] = [0.30, 0.32, 0.38, 1.0];
const TEXT_FG:         [f32; 4] = [0.95, 0.96, 0.97, 1.0];
const DIM_FG:          [f32; 4] = [0.60, 0.63, 0.70, 1.0];
const ACCENT_FG:       [f32; 4] = [0.40, 0.62, 1.00, 1.0];
const FOCUSED_BG:      [f32; 4] = [0.18, 0.28, 0.48, 1.0];
const DIVIDER_FG:      [f32; 4] = [0.22, 0.24, 0.28, 1.0];
const PANEL_RADIUS_PX: f32 = 10.0;
const ROW_RADIUS_PX: f32 = 5.0;
const SHADOW_BLUR_PX: f32 = 18.0;
const SHADOW_ALPHA: f32 = 0.45;

pub const OVERLAY_COLS: u16 = 40;
pub const LIST_MAX_ROWS: u16 = 10;

pub fn paint_search_overlay(p: &mut ViewPainter, params: SearchOverlayParams<'_>) {
    let overlay = params.overlay;
    if params.grid_cols < OVERLAY_COLS + 2 {
        return;
    }
    let cell_w = p.cell_w;
    let cell_h = p.cell_h;
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

    let panel = Panel::new(
        Rect {
            x: bar_x as f64,
            y_top: panel_y as f64,
            w: panel_w as f64,
            h: panel_h as f64,
        },
        ViewStyle {
            bg: PANEL_BG,
            border_color: PANEL_BORDER,
            border_width: 1.0,
            corner_radius: PANEL_RADIUS_PX,
            shadow_blur: SHADOW_BLUR_PX,
            shadow_alpha: SHADOW_ALPHA,
            backdrop: Backdrop::None,
            padding: 0.0,
        },
        panel_inner_pad as f64,
    );

    panel.paint(p, |p| {
        let content = panel.content_rect();
        let inner_left = content.x as f32;
        let inner_top = content.y_top as f32;

        // Query input row.  Reserve trailing space for × / Aa /
        // counter at the right.
        let trailing_reserve_chars = 12.0_f32;
        let input_w = (panel_w - 2.0 * panel_inner_pad
            - trailing_reserve_chars * cell_w).max(cell_w);
        let input = TextInput {
            rect: Rect {
                x: inner_left as f64,
                y_top: inner_top as f64,
                w: input_w as f64,
                h: cell_h as f64,
            },
            value: &overlay.query,
            cursor: overlay.query_cursor,
            style: TextInputStyle {
                fg: TEXT_FG,
                caret_color: ACCENT_FG,
                caret_w: 2.0,
                caret_inset_y: 2.0,
            },
        };
        input.paint(p);

        // Trailing buttons (× / Aa / counter).  × and Aa are real
        // Button widgets in ghost style (transparent BG when idle,
        // subtle fill on hover).  Counter stays as a free text run
        // since it's read-only label, not interactive.
        let query_baseline = inner_top + p.ascent;
        let close_x = bar_x + panel_w - panel_inner_pad - cell_w;
        let close_btn = Button {
            rect: Rect {
                x: close_x as f64,
                y_top: inner_top as f64,
                w: (cell_w * 1.5) as f64,
                h: cell_h as f64,
            },
            label: None,
            icon: Some(IconSpec::Glyph("×")),
            icon_position: IconPosition::Only,
            hovered: false,
            style: ButtonStyle::ghost(),
        };
        close_btn.paint(p);
        // Aa toggle: text-only button.  FG accents when case-sensitive
        // (on); dim otherwise — handled via a per-frame style mutation.
        //
        // Width measured, not counted: button labels are set in the
        // shared panel role now, and a proportional `Aa` is wider than
        // the two cells this used to reserve — it grew into the `×`
        // beside it (2026-08-11).  A control sized to its own label
        // cannot collide with its neighbour.
        let aa_w = p
            .panel_text_width(crate::ui::theme::PanelText::Label, "Aa")
            + cell_w;
        let aa_gap = cell_w * 0.75;
        let aa_x = close_x - aa_gap - aa_w;
        let mut aa_style = ButtonStyle::ghost();
        if overlay.case_sensitive {
            aa_style.fg = ACCENT_FG;
            aa_style.fg_hover = ACCENT_FG;
        }
        let aa_btn = Button {
            rect: Rect {
                x: aa_x as f64,
                y_top: inner_top as f64,
                w: aa_w as f64,
                h: cell_h as f64,
            },
            label: Some("Aa"),
            icon: None,
            icon_position: IconPosition::Only,
            hovered: false,
            style: aa_style,
        };
        aa_btn.paint(p);
        if let Some((c, t)) = overlay.counter {
            let counter_text = format!("{c}/{t}");
            let counter_w_chars = counter_text.chars().count() as f32;
            let counter_x = aa_x - (counter_w_chars + 1.0) * cell_w;
            p.text(counter_x, query_baseline, &counter_text, DIM_FG);
        }

        // Divider hairline + list rows.
        if has_list {
            let divider_y = inner_top + cell_h + (panel_inner_pad * 0.5).round();
            p.fill_rounded_rect(
                Rect {
                    x: inner_left as f64,
                    y_top: divider_y as f64,
                    w: (panel_w - 2.0 * panel_inner_pad) as f64,
                    h: 1.0,
                },
                DIVIDER_FG,
                0.5,
                ([0.0, 0.0, 0.0, 0.0], 0.0),
            );
            let list_top = divider_y + (panel_inner_pad * 0.5).round();
            let list_h = (grid_bottom - list_top).max(0.0);
            let rows: Vec<ListRow> = overlay.hits.iter().map(|h| {
                ListRow { label: &h.snippet, is_focused: h.is_focused }
            }).collect();
            let list_view = ListView {
                rect: Rect {
                    x: bar_x as f64,
                    y_top: list_top as f64,
                    w: panel_w as f64,
                    h: list_h as f64,
                },
                rows: &rows,
                row_h: cell_h,
                // Match the panel's inner left so list text aligns
                // with the query text.
                text_pad_left: panel_inner_pad,
                style: ListViewStyle {
                    fg: DIM_FG,
                    fg_focused: TEXT_FG,
                    focused_bg: FOCUSED_BG,
                    focused_radius: ROW_RADIUS_PX,
                    focused_inset_x: 4.0,
                },
            };
            list_view.paint(p);
        }
    });
}
