//! The layout modal: the data a frame of it is drawn from, and how it
//! is painted.  Its geometry and hit-testing are in `layout_modal`.

use marspot_term::layout::Alignment;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use golia_ui_core::scene::{RectInstance as CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::modal_frame::{MODAL_FRAME_BG, MODAL_FRAME_BORDER, MODAL_FRAME_CORNER_RADIUS};
use crate::ui::components::panel_palette;

/// F3+3.0 / 3.3 — full data for one render of the LayoutModal.
/// `set_layout_modal(Some(_))` toggles it on, with the per-slot
/// titles + active drag info needed to paint cards.
#[derive(Debug, Clone)]
pub struct LayoutModalRender {
    pub cols: usize,
    pub rows: usize,
    pub scale: f64,
    /// One title per slot, in slot order (length = cols * rows).
    /// Empty string = empty slot (no card content drawn).
    pub slot_titles: Vec<String>,
    /// Drag state if a card drag is in progress this frame.
    pub drag: Option<LayoutModalDragRender>,
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutModalDragRender {
    pub from_slot: usize,
    pub grab_offset_phys: (f64, f64),
    pub mouse_phys: (f64, f64),
}

/// F3+3.0 — paint the `LayoutModal` overlay.  Same plumbing as
/// `push_process_panel_via_view`: routes through a `ViewPainter`
/// → overlay scratches so it lands on top of the grid.  Geometry
/// is computed from the modal's own `LayoutModal::layout` (cols
/// × rows steppers + Apply + footer text are all positional).
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_layout_modal_via_view(
    state: &LayoutModalRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    use crate::ui::components::LayoutModal;
    // The widest label decides how wide a card wants to be; the
    // renderer is where both the labels and the cell metrics are.
    let widest_label = state
        .slot_titles
        .iter()
        .map(|t| t.chars().count())
        .max()
        .unwrap_or(0) as f64
        * cell_w as f64;
    let modal = LayoutModal::layout(
        window_w, window_h, state.scale, top_inset,
        state.cols, state.rows, widest_label,
    );
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: modal.frame,
        style: ViewStyle {
            bg: MODAL_FRAME_BG,
            border_color: MODAL_FRAME_BORDER,
            border_width: 1.0,
            corner_radius: MODAL_FRAME_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: Backdrop::Dim {
                color: [0.0, 0.0, 0.0, 0.45],
                exclude_above_y: top_inset,
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_layout_modal_content(&modal, state, p);
    });
}

/// Internal paint of the modal's content (title text, close X,
/// stepper buttons + values, footer total, Apply button).  All
/// rects come pre-computed from `LayoutModal::layout`; here we
/// just draw atop them.
fn paint_layout_modal_content(
    modal: &crate::ui::components::LayoutModal,
    state: &LayoutModalRender,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    let pending_cols = state.cols;
    let pending_rows = state.rows;
    let scale = state.scale;
    use crate::ui::components::{Button, ButtonStyle, IconSpec, IconPosition};
    // Colors mirror process panel for visual consistency.
    let stepper_bg = [0.18, 0.20, 0.24, 1.0];
    let stepper_bg_hover = [0.24, 0.26, 0.30, 1.0];
    let stepper_fg = [0.85, 0.88, 0.92, 1.0];
    let title_fg = [0.85, 0.88, 0.92, 1.0];
    let muted_fg = [0.55, 0.60, 0.66, 1.0];
    let apply_bg = [0.20, 0.42, 0.68, 1.0];
    let apply_fg = [0.95, 0.97, 1.0, 1.0];
    let stepper_style = ButtonStyle {
        bg: stepper_bg,
        bg_hover: stepper_bg_hover,
        fg: stepper_fg,
        fg_hover: stepper_fg,
        border_color: [0.0; 4],
        border_width: 0.0,
        corner_radius: 4.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: (12.0 * scale) as f32,
    };
    // F3+3.4 — `text_in(rect, s, color, align)` replaces all the
    // hand-rolled `(rect.h - cell_h) * 0.5 + ascent` math below.
    // Title text — left-pad'd, vertically centered in title_bar.
    let title_pad_left = 14.0 * scale;
    let title_inner = marspot_term::layout::Rect {
        x: modal.title_bar.x + title_pad_left,
        y_top: modal.title_bar.y_top,
        w: modal.title_bar.w - title_pad_left,
        h: modal.title_bar.h,
    };
    p.panel_text_in(
        crate::ui::theme::PanelText::Title,
        title_inner,
        "Layout",
        title_fg,
        Alignment::CenterLeft,
    );
    // Close [×] glyph, fully centered in its hit-target.
    p.text_in(modal.close_btn, "×", muted_fg, Alignment::Center);
    // Stepper buttons: cols [-] [+], rows [-] [+].
    for (rect, label) in [
        (modal.cols_dec, "−"),
        (modal.cols_inc, "+"),
        (modal.rows_dec, "−"),
        (modal.rows_inc, "+"),
    ] {
        let btn = Button {
            rect,
            label: Some(label),
            icon: None as Option<IconSpec>,
            icon_position: IconPosition::Only,
            hovered: false,
            style: stepper_style,
        };
        btn.paint(p);
    }
    // Stepper VALUES — N inside cols_value / rows_value, centered.
    let cols_str = pending_cols.to_string();
    let rows_str = pending_rows.to_string();
    p.text_in(modal.cols_value, &cols_str, title_fg, Alignment::Center);
    p.text_in(modal.rows_value, &rows_str, title_fg, Alignment::Center);
    // Row labels — left-aligned with same vertical baseline as the
    // adjacent value cell.  Use the value rect as the y reference
    // (so they're guaranteed visually aligned), but x = body left
    // pad of the modal frame.
    let label_pad = 16.0 * scale;
    let cols_label_rect = marspot_term::layout::Rect {
        x: modal.frame.x + label_pad,
        y_top: modal.cols_value.y_top,
        w: modal.cols_value.x - modal.frame.x - label_pad,
        h: modal.cols_value.h,
    };
    let rows_label_rect = marspot_term::layout::Rect {
        x: modal.frame.x + label_pad,
        y_top: modal.rows_value.y_top,
        w: modal.rows_value.x - modal.frame.x - label_pad,
        h: modal.rows_value.h,
    };
    // Prose labels take the shared Label role; the numbers between
    // them stay mono, where digits keep their column.
    p.panel_text_in(
        crate::ui::theme::PanelText::Label,
        cols_label_rect, "Columns", title_fg, Alignment::CenterLeft,
    );
    p.panel_text_in(
        crate::ui::theme::PanelText::Label,
        rows_label_rect, "Rows", title_fg, Alignment::CenterLeft,
    );
    // Footer: "Total: N panes" centered.
    let total = pending_cols * pending_rows;
    let total_str = format!("Total: {}×{} = {} pane{}",
        pending_cols, pending_rows, total,
        if total == 1 { "" } else { "s" });
    // Prose, not a column of figures — the secondary role, like the
    // sentence under a settings row.
    p.panel_text_in(
        crate::ui::theme::PanelText::Secondary,
        modal.total_label,
        &total_str,
        muted_fg,
        Alignment::Center,
    );
    // Apply button — full-width blue, white "Apply" label.
    let apply_style = ButtonStyle {
        bg: apply_bg,
        bg_hover: apply_bg,
        fg: apply_fg,
        fg_hover: apply_fg,
        border_color: [0.0; 4],
        border_width: 0.0,
        corner_radius: 4.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: 0.0,
    };
    let apply_btn = Button {
        rect: modal.apply_btn,
        label: Some("Apply"),
        icon: None as Option<IconSpec>,
        icon_position: IconPosition::Only,
        hovered: false,
        style: apply_style,
    };
    apply_btn.paint(p);
    paint_layout_cards(modal, state, p);
}

/// The card grid and the drag overlay: one card per slot with its
/// pane's title, the hovered drop target, and the lifted card on top.
fn paint_layout_cards(
    modal: &crate::ui::components::LayoutModal,
    state: &LayoutModalRender,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    // Cache atlas-driven metrics needed by the card-paint block.
    let cell_w = p.cell_w;
    let cell_h = p.cell_h;
    let ascent = p.ascent;
    // F3+3.3 — card grid + drag overlay.  Each slot renders a
    // small rounded card with its pane's title centered.  Order
    // matters: BG cards first, then drop-target highlight on the
    // hovered slot, then the dragged card on top so it floats
    // above everything.
    // Theme tokens, not hand-mixed RGB: a card here has to be the
    // same object as a card in the settings panel, and six literals
    // are six chances for it not to be.
    let card_bg = panel_palette::card_bg();
    // The slot you lifted a pane out of — recessed to the base
    // surface, so it reads as a hole rather than a card.
    let card_bg_drag_origin = crate::ui::theme::token::color::BG.to_rgba_f32();
    let card_bg_drop_target = crate::ui::theme::token::color::BG_SELECTED.to_rgba_f32();
    let card_border = panel_palette::card_border();
    let card_fg = panel_palette::fg();
    let card_fg_drag_origin = panel_palette::fg_faint();
    // Drop target: only highlighted while a drag is active AND
    // the drop target differs from the source slot.
    let drop_target_slot: Option<usize> = state.drag.as_ref().and_then(|d| {
        let cw = modal.cards.first().map(|c| c.w).unwrap_or(0.0);
        let ch = modal.cards.first().map(|c| c.h).unwrap_or(0.0);
        let cx = d.mouse_phys.0 - d.grab_offset_phys.0 + cw * 0.5;
        let cy = d.mouse_phys.1 - d.grab_offset_phys.1 + ch * 0.5;
        modal.nearest_card(cx, cy).filter(|&s| s != d.from_slot)
    });
    for (slot, rect) in modal.cards.iter().enumerate() {
        let is_drag_origin = state
            .drag
            .as_ref()
            .map(|d| d.from_slot == slot)
            .unwrap_or(false);
        let is_drop_target = drop_target_slot == Some(slot);
        let bg = if is_drag_origin {
            card_bg_drag_origin
        } else if is_drop_target {
            card_bg_drop_target
        } else {
            card_bg
        };
        p.fill_rounded_rect(*rect, bg, 6.0, (card_border, 1.0));
        // Title centred in the card, cut to what the card holds.
        // Project names are as long as their directory (`lab36-
        // continus`, `lab38-golialab`) and a card is as wide as the
        // grid leaves it, so "draw it and hope" put 101 px of name in
        // an 80 px card and let the rest run over its neighbour.
        if let Some(title) = state.slot_titles.get(slot)
            && !title.is_empty() {
                let fg = if is_drag_origin { card_fg_drag_origin } else { card_fg };
                let pad = crate::ui::components::layout_modal::CARD_LABEL_PAD_LOGICAL
                    * state.scale;
                let fitted = crate::ui::core::fit_mono(
                    title, rect.w, p.cell_w as f64, pad,
                );
                p.text_in(*rect, &fitted, fg, Alignment::Center);
            }
    }
    let _ = (cell_w, cell_h, ascent);
    // Floating dragged card: a copy of the source card painted at
    // (mouse - grab_offset).  Drawn LAST so it sits on top of all
    // other cards.  Same BG / border as a regular card but more
    // saturated to read as "lifted".
    if let Some(d) = state.drag.as_ref()
        && d.from_slot < modal.cards.len() {
            let src = modal.cards[d.from_slot];
            let drag_rect = marspot_term::layout::Rect {
                x: d.mouse_phys.0 - d.grab_offset_phys.0,
                y_top: d.mouse_phys.1 - d.grab_offset_phys.1,
                w: src.w,
                h: src.h,
            };
            let drag_bg = [0.22, 0.26, 0.32, 1.0];
            let drag_border = [0.55, 0.62, 0.72, 1.0];
            p.fill_rounded_rect(drag_rect, drag_bg, 6.0, (drag_border, 1.5));
            if let Some(title) = state.slot_titles.get(d.from_slot)
                && !title.is_empty() {
                    p.text_in(drag_rect, title, card_fg, Alignment::Center);
                }
        }
}
