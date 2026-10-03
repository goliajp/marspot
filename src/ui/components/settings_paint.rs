//! The settings panel: the data a frame of it is drawn from, and how
//! it is painted.
//!
//! Every row is walked by `settings_modal::walk_visible` -- the same
//! walker the hit-test uses, which is what makes a control clickable
//! exactly where it is drawn.

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::render_metal::{CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::modal_frame::{MODAL_FRAME_BG, MODAL_FRAME_BORDER, MODAL_FRAME_CORNER_RADIUS};
use crate::ui::components::panel_palette;

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
}

/// The settings panel.
///
/// Every row is label / cost / control, walked by
/// `settings_modal::walk` — the same walker the hit-test uses, which
/// is what makes a control clickable exactly where it is drawn.
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
    use crate::ui::components::settings_modal as sm;
    use crate::ui::core::view::{Backdrop, View, ViewPainter, ViewStyle};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: sp.rect,
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
        use crate::ui::components::settings_modal::Slot;
        let px = ViewPainter::px_per_pt();
        // The walker needs a measurer and the painter owns the font,
        // so collect the geometry first and paint from it.  Both this
        // and the hit-test measure through the same path, which is
        // what keeps a segment as wide as the label inside it.
        let mut slots: Vec<Slot> = Vec::new();
        {
            let font = &mut *p.font;
            let mut m = |s: &str, pt: f64, w: u16| {
                font.measure_ui_text_at_size(
                    s, w, crate::font_shape::ShapeOptions::default(), pt,
                )
            };
            sm::walk_visible(sp.rect, sp.scroll, &sp.settings, &mut m, |slot| slots.push(slot));
        }

        for slot in slots {
            paint_slot(p, sp, px, slot);
        }
    });
}

/// One slot of the settings panel: the title, a section heading, a
/// row's label and cost, or its control.
fn paint_slot(
    p: &mut crate::ui::core::view::ViewPainter,
    sp: &SettingsRender,
    px: f64,
    slot: crate::ui::components::settings_modal::Slot,
) {
    use crate::ui::components::settings_modal as sm;
    use crate::ui::components::settings_modal::{Control, Slot};
    match slot {
        Slot::Title { baseline } => {
            p.ui_text_at(
                sm::text_x(sp.rect) as f32,
                baseline as f32,
                "Settings",
                sm::metric::TITLE.pt(),
                sm::metric::TITLE.weight(),
                panel_palette::fg(),
            );
        }
        Slot::Group { heading, baseline } => {
            p.ui_text_at(
                sm::text_x(sp.rect) as f32,
                baseline as f32,
                heading,
                sm::metric::GROUP.pt(),
                sm::metric::GROUP.weight(),
                panel_palette::fg(),
            );
        }
        Slot::Card { rect } => {
            // The card is what makes a group read as a group.
            p.fill_rounded_rect(
                rect,
                panel_palette::card_bg(),
                (sm::metric::CARD_RADIUS * px) as f32,
                (panel_palette::card_border(), 1.0),
            );
        }
        Slot::Separator { rect } => {
            // `fill_rounded_rect`, not `fill_rect`: the two go
            // to different pipelines, and the whole cells pass
            // is drawn *before* the whole ui_rects pass — so a
            // hairline submitted after the card was painted
            // under it and vanished.  Same pass, later
            // submission, visible.
            p.fill_rounded_rect(rect, panel_palette::separator(), 0.0, ([0.0; 4], 0.0));
        }
        Slot::Row { row, spec, label_baseline, desc_baseline, control, .. } => {
            let dim = row.disabled_by(&sp.settings);
            let (fg, sec) = if dim {
                (panel_palette::fg_faint(), panel_palette::fg_faint())
            } else {
                (panel_palette::fg(), panel_palette::fg_muted())
            };
            let x = sm::text_x(sp.rect) as f32;
            p.ui_text_at(
                x, label_baseline as f32, crate::ui::strings::t(spec.label),
                sm::metric::LABEL.pt(), sm::metric::LABEL.weight(), fg,
            );
            // Genuinely smaller, not merely dimmer: same size
            // in a paler grey is two competing lines.
            p.ui_text_at(
                x, desc_baseline as f32, crate::ui::strings::t(spec.cost),
                sm::metric::DESC.pt(), sm::metric::DESC.weight(), sec,
            );
            match row.control(&sp.settings) {
                Control::Toggle(on) => {
                    // A pill with the knob at one end.  Drawn,
                    // not written: a checkbox at this size
                    // reads as decoration, a pill as a switch.
                    let r = control.h * 0.5;
                    let bg = if !dim && on {
                        panel_palette::ok()
                    } else {
                        panel_palette::track()
                    };
                    p.fill_rounded_rect(control, bg, r as f32, ([0.0; 4], 0.0));
                    let knob_d = control.h * 0.74;
                    let pad = (control.h - knob_d) * 0.5;
                    let kx = if on {
                        control.x + control.w - knob_d - pad
                    } else {
                        control.x + pad
                    };
                    p.fill_rounded_rect(
                        marspot_term::layout::Rect {
                            x: kx,
                            y_top: control.y_top + pad,
                            w: knob_d,
                            h: knob_d,
                        },
                        panel_palette::fg(),
                        (knob_d * 0.5) as f32,
                        ([0.0; 4], 0.0),
                    );
                }
                Control::Segmented { options, chosen } => {
                    for (n, label) in options.iter().enumerate() {
                        let seg = {
                            let font = &mut *p.font;
                            let mut m = |s: &str, pt: f64, w: u16| {
                                font.measure_ui_text_at_size(
                                    s, w,
                                    crate::font_shape::ShapeOptions::default(),
                                    pt,
                                )
                            };
                            sm::segment_rect(control, n, options, &mut m)
                        };
                        let picked = !dim && n == chosen;
                        let (bg, border, fg) = if picked {
                            (panel_palette::ok(), panel_palette::ok(), [1.0, 1.0, 1.0, 1.0])
                        } else if dim {
                            (panel_palette::segment_bg(), panel_palette::card_border(), panel_palette::fg_faint())
                        } else {
                            (panel_palette::segment_bg(), panel_palette::card_border(), panel_palette::fg())
                        };
                        p.fill_rounded_rect(
                            seg, bg, (5.0 * px) as f32, (border, 1.0),
                        );
                        // Measured, then centred on what was
                        // measured — the label overran its
                        // button when width came from a count.
                        let tw = p.ui_text_width_at(
                            label, sm::metric::SEGMENT.pt(), sm::metric::SEGMENT.weight(),
                        );
                        let baseline = p.ui_baseline_centred(
                            seg.y_top as f32, seg.h as f32, sm::metric::SEGMENT.pt(),
                        );
                        p.ui_text_at(
                            (seg.x + (seg.w - tw as f64) * 0.5) as f32,
                            baseline,
                            label,
                            sm::metric::SEGMENT.pt(),
                            sm::metric::SEGMENT.weight(),
                            fg,
                        );
                    }
                }
            }
        }
        Slot::Footer { baseline } => {
            // Where the file is.  The panel is one way to edit
            // it, not the only one.
            p.ui_text_at(
                sm::text_x(sp.rect) as f32,
                baseline as f32,
                &sp.path,
                sm::metric::FOOTER.pt(),
                sm::metric::FOOTER.weight(),
                panel_palette::fg_faint(),
            );
        }
    }
}
