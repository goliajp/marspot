//! The layout panel: the data a frame of it is drawn from, and how it
//! is painted.
//!
//! The frame and header are the shared panel chrome's; everything
//! under them is drawn on the rects `LayoutModal::layout` returns —
//! the same ones the click handler hit-tests.

use marspot_term::layout::Rect;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::components::controls::{self, ButtonKind};
use crate::ui::components::layout_modal::{
    self as lm, metric, Hover, LayoutModal, GRID_MAX, GRID_MIN,
};
use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, card};
use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance, UiRectInstance};

/// One frame of the layout panel: the pending grid shape, what each
/// slot holds, and what the pointer is doing.
#[derive(Debug, Clone, Default)]
pub struct LayoutModalRender {
    pub cols: usize,
    pub rows: usize,
    /// One title per slot, in slot order (length = cols * rows).
    /// Empty string = empty slot (no card content drawn).
    pub slot_titles: Vec<String>,
    /// Drag state if a card drag is in progress this frame.
    pub drag: Option<LayoutModalDragRender>,
    /// What the pointer is on, from `LayoutModal::hover_at`.
    pub hover: Hover,
    /// How far the panel's header has been dragged from the middle.
    pub offset: (f64, f64),
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutModalDragRender {
    pub from_slot: usize,
    pub grab_offset_phys: (f64, f64),
    pub mouse_phys: (f64, f64),
}

/// The layout panel, on the overlay layers so it lands above the grid.
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
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    // Measured the way the click handler measures, so both arrive at
    // the same panel.
    let modal = {
        let mut m = controls::font_measure(&mut *painter.font);
        let label_w = lm::widest_label(&state.slot_titles, &mut m);
        LayoutModal::layout_moved(
            window_w, window_h, top_inset, state.cols, state.rows, label_w, &mut m, state.offset,
        )
    };
    modal.chrome.paint(&mut painter, t(lm::TITLE), None, state.hover == Hover::Close, |p, content| {
        paint_grid_group(p, &modal, state, content);
        panel_chrome::section_heading(
            p, content.x, modal.arrangement_heading_top, content.w, t(lm::ARRANGEMENT_HEADING), None,
        );
        paint_cards(p, &modal, state);
        let baseline = p.ui_baseline_centred(
            modal.footer.y_top as f32, modal.footer.h as f32, metric::CAPTION.pt(),
        );
        p.panel_text(
            metric::CAPTION,
            modal.footer.x as f32,
            baseline,
            &lm::caption(state.cols, state.rows),
            palette::fg_muted(),
        );
        controls::paint_button(
            p, modal.apply_btn, t(lm::APPLY), ButtonKind::Primary, state.hover == Hover::Apply, false,
        );
        paint_lifted_card(p, &modal, state);
    });
}

/// The Grid group: its heading and a card with one stepper row per
/// axis.
fn paint_grid_group(p: &mut ViewPainter, modal: &LayoutModal, state: &LayoutModalRender, content: Rect) {
    panel_chrome::section_heading(
        p, content.x, modal.grid_heading_top, content.w, t(lm::GRID_HEADING), None,
    );
    panel_chrome::card(p, modal.grid_card);
    for (row, value) in modal.rows.iter().zip([state.cols, state.rows]) {
        let baseline = p.ui_baseline_centred(
            row.band.y_top as f32, row.band.h as f32, metric::ROW_LABEL.pt(),
        );
        p.panel_text(
            metric::ROW_LABEL,
            (row.band.x + spec::px(card::ROW_PAD_X)) as f32,
            baseline,
            t(row.axis.label()),
            palette::fg(),
        );
        let hovered = match state.hover {
            Hover::Step(axis, part) if axis == row.axis => Some(part),
            _ => None,
        };
        controls::paint_stepper(
            p,
            row.stepper.minus.x,
            row.stepper.minus.y_top,
            &value.to_string(),
            value > GRID_MIN,
            value < GRID_MAX,
            hovered,
        );
    }
    panel_chrome::row_separator(p, modal.grid_card, modal.rows[1].band.y_top);
}

/// A pane card in a state other than at rest.
fn fill_card(p: &mut ViewPainter, rect: Rect, bg: [f32; 4], border: [f32; 4]) {
    p.fill_rounded_rect(rect, bg, spec::px(card::RADIUS) as f32, (border, card::BORDER_PX));
}

/// `title` centred in the pane card at `rect`, cut to what the card
/// holds.  Left out where the card being dragged (`lifted`) is over
/// it: text is drawn above every rect, so it would show through.
fn card_label(p: &mut ViewPainter, rect: Rect, title: &str, color: [f32; 4], lifted: Option<Rect>) {
    let role = metric::CARD_LABEL;
    let line_h = spec::px(role.cap() + role.descent());
    // A card squeezed thinner than a line of text holds none.
    if title.is_empty() || rect.h < line_h {
        return;
    }
    let fitted = {
        let mut m = controls::font_measure(&mut *p.font);
        lm::fit_label(title, lm::label_room(rect.w), &mut m)
    };
    let w = p.panel_text_width(role, &fitted) as f64;
    let x = rect.x + (rect.w - w) * 0.5;
    let baseline = p.ui_baseline_centred_for(&fitted, rect.y_top as f32, rect.h as f32, role.pt());
    let top = baseline as f64 - spec::px(role.cap());
    let covered = lifted.is_some_and(|l| {
        x < l.x + l.w && l.x < x + w && top < l.y_top + l.h && l.y_top < top + line_h
    });
    if !covered {
        p.panel_text(role, x as f32, baseline, &fitted, color);
    }
}

/// One card per slot with its pane's name.  While a card is being
/// dragged, the slot it came from is drawn as a hole and the slot it
/// would land in takes the accent.
fn paint_cards(p: &mut ViewPainter, modal: &LayoutModal, state: &LayoutModalRender) {
    let origin = state.drag.map(|d| d.from_slot);
    let target = state
        .drag
        .and_then(|d| modal.drop_target(d.from_slot, d.mouse_phys, d.grab_offset_phys));
    let lifted = lifted_rect(modal, state);
    for (slot, rect) in modal.cards.iter().enumerate() {
        let fg = if origin == Some(slot) {
            // The panel's own surface: a hole where the card was.
            fill_card(p, *rect, palette::frame_bg(), palette::card_border());
            palette::fg_faint()
        } else if target == Some(slot) {
            fill_card(p, *rect, palette::accent(), palette::accent());
            palette::fg()
        } else {
            panel_chrome::card(p, *rect);
            palette::fg()
        };
        if let Some(title) = state.slot_titles.get(slot) {
            card_label(p, *rect, title, fg, lifted);
        }
    }
}

fn lifted_rect(modal: &LayoutModal, state: &LayoutModalRender) -> Option<Rect> {
    state.drag.and_then(|d| modal.lifted_card(d.from_slot, d.mouse_phys, d.grab_offset_phys))
}

/// The card being dragged, under the pointer.  Painted after
/// everything else so it floats over the cards and the footer.
fn paint_lifted_card(p: &mut ViewPainter, modal: &LayoutModal, state: &LayoutModalRender) {
    let (Some(d), Some(rect)) = (state.drag, lifted_rect(modal, state)) else { return };
    // A level above the cards it passes over, outlined so its edge
    // stays visible against them.
    fill_card(p, rect, palette::segment_bg(), palette::fg_muted());
    if let Some(title) = state.slot_titles.get(d.from_slot) {
        card_label(p, rect, title, palette::fg(), None);
    }
}
