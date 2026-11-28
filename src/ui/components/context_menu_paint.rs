//! How the context menu's parts are painted: its frame, its rows, its
//! dividers and the box that says why a row is off.

use crate::frame_build::color::rgba8_of_f32;
use crate::glyph_atlas::GlyphKey;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::theme::PanelText;
use crate::ui::theme::spec::{card, chrome, panel};
use golia_ui_core::WidgetState;
use golia_ui_core::scene::UiRectInstance;
use marspot_term::layout::Rect;

/// Between a row's label and its shortcut in the node's text.
pub(super) const HINT_MARK: char = '\u{1f}';
/// The label's ascent as a fraction of its point size: what the row
/// centres the capitals against.
const LABEL_ASCENT_RATIO: f64 = 0.75;

/// The menu's frame: the fill, the hairline round it and the shadow
/// under it.
pub(crate) fn paint_frame(p: &mut ViewPainter<'_>, r: Rect) {
    p.ui_rects.push(UiRectInstance {
        origin: [r.x as f32, r.y_top as f32],
        size: [r.w as f32, r.h as f32],
        fill: rgba8_of_f32(palette::frame_bg()),
        border: rgba8_of_f32(palette::frame_border()),
        radius: chrome::px(chrome::POPOVER_RADIUS) as f32,
        border_width: panel::BORDER_PX,
        shadow_offset: [0.0, 0.0],
        shadow_color: rgba8_of_f32(palette::frame_shadow()),
        shadow_blur: chrome::px(chrome::POPOVER_SHADOW_BLUR) as f32,
    });
}

/// A divider: the frame's own border colour across the middle of its
/// row.
pub(crate) fn paint_divider(p: &mut ViewPainter<'_>, r: Rect) {
    let pad = chrome::px(chrome::MENU_PAD_X);
    let (from, to) = (r.x + pad, r.x + r.w - pad);
    let line = Rect { x: from, y_top: r.y_top + r.h * 0.5 - card::SEPARATOR_PX * 0.5, w: to - from, h: card::SEPARATOR_PX };
    p.fill_rounded_rect(line, palette::frame_border(), 0.0, (palette::none(), 0.0));
}

/// Why a disabled row is off: a frame as the menu's, holding one row of
/// text.
pub(crate) fn paint_tip(p: &mut ViewPainter<'_>, r: Rect, why: &str) {
    paint_frame(p, r);
    let row = Rect { y_top: r.y_top + chrome::px(chrome::MENU_PAD_Y), h: chrome::px(chrome::MENU_ROW_H), ..r };
    paint_row_lit(p, row, why, WidgetState::IDLE);
}

/// A row: the band behind it while the pointer or the keyboard is on
/// it, or while its menu beside is open (`open`), its label, and its
/// shortcut against the right edge.
pub(crate) fn paint_row(p: &mut ViewPainter<'_>, r: Rect, text: &str, state: WidgetState, open: bool) {
    paint_row_lit(p, r, text, WidgetState { hovered: state.hovered || open, ..state });
}

/// A row: the band behind it while the pointer is on it, its label,
/// and its shortcut against the right edge.
fn paint_row_lit(p: &mut ViewPainter<'_>, r: Rect, text: &str, state: WidgetState) {
    let (label, hint) = text.split_once(HINT_MARK).unwrap_or((text, ""));
    let pad = chrome::px(chrome::MENU_PAD_X);
    if state.hovered || state.focused {
        let inset = chrome::px(chrome::POPOVER_ROW_INSET);
        let band = Rect { x: r.x + inset, y_top: r.y_top, w: r.w - 2.0 * inset, h: r.h };
        p.fill_rounded_rect(band, palette::row_hover(), chrome::px(chrome::POPOVER_ROW_RADIUS) as f32, (palette::none(), 0.0));
    }
    // The label is prose, in the role every panel sets an item in.
    // Its capitals are centred on the row from the top of its em box:
    // `centre - (ascent - cap / 2)`, then down by the ascent the font
    // itself reports.
    let role = PanelText::Item;
    let px = ViewPainter::px_per_pt();
    let size_pt = GlyphKey::size_q_for(role.pt()) as f64 / 4.0;
    let top = r.y_top + r.h * 0.5 - (LABEL_ASCENT_RATIO * role.pt() - role.cap() * 0.5) * px;
    let opts = crate::font_shape::ShapeOptions::default();
    if let Some(at) = p.font.shape_ui_kept(label, role.weight(), opts, size_pt) {
        let face = p.font.shaped_ui(label, at, opts).first().map_or(0, |g| g.font_id as usize);
        let ascent = (p.font.font_ascent(face) * px) as f32;
        let fg = if state.disabled { palette::disabled_fg() } else { palette::fg() };
        p.ui_text_at((r.x + pad) as f32, top as f32 + ascent, label, size_pt, role.weight(), fg);
    }
    if !hint.is_empty() {
        // a cluster of key symbols, not prose: in the mono cell, where
        // each keeps its own width
        let w = hint.chars().count() as f64 * p.cell_w as f64;
        let top = r.y_top + (r.h - p.cell_h as f64) * 0.5;
        p.text((r.x + r.w - pad - w) as f32, top as f32 + p.ascent, hint, palette::fg_muted());
    }
}

