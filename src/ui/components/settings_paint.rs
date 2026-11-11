//! The settings panel: the data a frame of it is drawn from, and how
//! it is painted.
//!
//! The frame and header are the shared panel chrome's; every row is
//! walked by `settings_modal::walk_visible` -- the same walker the
//! hit-test uses, which is what makes a control clickable exactly
//! where it is drawn.

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use golia_ui_core::scene::{RectInstance as CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::panel_palette;
use crate::ui::components::settings_modal::{self as sm, Control, Hover, Slot};
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::Msg;

#[derive(Debug, Clone)]
/// The settings panel's render data — its rect and the values in
/// force when the frame was built.
///
/// A snapshot, not a live read: the painter must draw one consistent
/// set of values, and re-reading mid-panel could show a toggle from
/// before a click and a segment from after it.
pub struct SettingsRender {
    pub rect: marspot_term::layout::Rect,
    /// How far the content is pushed up inside `rect`.  Nonzero only
    /// when the window is too short to show the panel whole.
    pub scroll: f64,
    pub settings: crate::settings::Settings,
    /// Shown in the footer so the file is findable.
    pub path: String,
    /// What the pointer is on, from `settings_modal::hover_at`.
    pub hover: Hover,
}

/// The settings panel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_settings_panel_via_view(
    sp: &SettingsRender,
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
    let chrome = PanelChrome::new(sp.rect, top_inset);
    chrome.paint(&mut painter, crate::ui::strings::t(Msg::PanelSettings), None, sp.hover == Hover::Close, |p, _content| {
        // The walker needs a measurer and the painter owns the font,
        // so collect the geometry first and paint from it.  Both this
        // and the hit-test measure through the same path, which is
        // what keeps a segment as wide as the label inside it.
        let mut slots: Vec<Slot> = Vec::new();
        {
            let mut m = controls::font_measure(&mut *p.font);
            sm::walk_visible(sp.rect, sp.scroll, &sp.settings, &mut m, |slot| slots.push(slot));
        }
        for slot in slots {
            paint_slot(p, sp, slot);
        }
    });
}

/// One slot of the settings panel: a group heading, its card, a row's
/// label, cost and control, or the footer.
fn paint_slot(p: &mut ViewPainter, sp: &SettingsRender, slot: Slot) {
    match slot {
        Slot::Group { heading, x, top, w } => {
            panel_chrome::section_heading(p, x, top, w, heading, None);
        }
        // The card is what makes a group read as a group.
        Slot::Card { rect } => panel_chrome::card(p, rect),
        Slot::Separator { card, y } => panel_chrome::row_separator(p, card, y),
        Slot::Row { row, spec, label_baseline, desc_baseline, control, .. } => {
            let dim = row.disabled_by(&sp.settings);
            let (fg, sec) = if dim {
                (panel_palette::fg_faint(), panel_palette::fg_faint())
            } else {
                (panel_palette::fg(), panel_palette::fg_muted())
            };
            let x = sm::text_x(sp.rect) as f32;
            p.panel_text(
                sm::metric::LABEL, x, label_baseline as f32, crate::ui::strings::t(spec.label), fg,
            );
            // Genuinely smaller, not merely dimmer: same size
            // in a paler grey is two competing lines.
            p.panel_text(
                sm::metric::DESC, x, desc_baseline as f32, crate::ui::strings::t(spec.cost), sec,
            );
            match row.control(&sp.settings) {
                Control::Toggle(on) => controls::paint_toggle(p, control, on, dim),
                Control::Segmented { options, chosen } => {
                    let hovered = match sp.hover {
                        Hover::Control(r, n) if r == row => Some(n),
                        _ => None,
                    };
                    // `chosen` is out of range when the file holds a
                    // value the panel has no segment for.
                    let selected = (chosen < options.len()).then_some(chosen);
                    controls::paint_segmented(p, control, options, selected, hovered, dim);
                }
            }
        }
        Slot::Footer { baseline } => {
            // Where the file is.  The panel is one way to edit
            // it, not the only one.
            let x = PanelChrome::new(sp.rect, 0.0).content.x as f32;
            p.panel_text(sm::metric::FOOTER, x, baseline as f32, &sp.path, panel_palette::fg_faint());
        }
    }
}
