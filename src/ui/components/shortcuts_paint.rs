//! The keyboard shortcuts panel: what a frame of it is drawn from,
//! and how it is painted.
//!
//! The frame and header are the shared panel chrome's; everything in
//! the content area is placed by `shortcuts_panel::walk`.

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::panel_palette;
use crate::ui::components::shortcuts_panel::{self as sp, Slot};
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::{t, Msg};
use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance, UiRectInstance};

/// The panel's render data: its frame, and whether the pointer is on
/// its close button.  The rows are the command table's and need no
/// snapshot.
#[derive(Debug, Clone, Copy)]
pub struct ShortcutsRender {
    pub rect: marspot_term::layout::Rect,
    pub close_hovered: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_shortcuts_panel_via_view(
    panel: &ShortcutsRender,
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
    let chrome = PanelChrome::new(panel.rect, top_inset);
    chrome.paint(&mut painter, t(Msg::PanelShortcuts), None, panel.close_hovered, |p, _content| {
        let cell = (p.cell_w as f64, p.cell_h as f64);
        sp::walk(panel.rect, cell, |slot| match slot {
            Slot::Group { heading, x, top, w } => {
                panel_chrome::section_heading(p, x, top, w, t(heading), None);
            }
            Slot::Card { rect } => panel_chrome::card(p, rect),
            Slot::Separator { card, y } => panel_chrome::row_separator(p, card, y),
            Slot::Row { title, title_x, title_baseline, caps } => {
                p.panel_text(
                    sp::metric::TITLE,
                    title_x as f32,
                    title_baseline as f32,
                    t(title),
                    panel_palette::fg(),
                );
                for cap in &caps {
                    if cap.filled {
                        controls::paint_keycap(p, cap.rect, &cap.label);
                    } else {
                        controls::paint_keycap_text(p, cap.rect, &cap.label);
                    }
                }
            }
        });
    });
}
