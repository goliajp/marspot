//! Text measured for the tree by the font cache the painters use.
//!
//! Every answer is the number a painter would have worked out for the
//! same string, to the bit: a run laid out by the tree and one placed
//! by hand have to land on the same pixel, and they do because they
//! were measured by the same code.

use std::cell::RefCell;

use crate::font_cache::{FontCache, UI_FONT_POINT};
use crate::font_shape::ShapeOptions;
use crate::ui::theme::type_scale::{UiSize, sf_pro_cap_height};
use golia_ui_core::text::size_q;
use golia_ui_core::{Extent, Fit, FontMetrics, FontSpec, TextMeasure};

/// The point size `spec` stands for.
///
/// A spec carries its size in quarter points, and no rung of the type
/// ladder is on one: the ladder is defined by cap height.  A spec that
/// rounds to a rung is that rung, at its exact size, so a role's text
/// is shaped at the size every other painter shapes it at.
pub fn exact_pt(spec: FontSpec) -> f64 {
    const LADDER: [UiSize; 6] =
        [UiSize::Mini, UiSize::Small, UiSize::Body, UiSize::Heading, UiSize::Title, UiSize::Display];
    LADDER
        .iter()
        .map(|rung| rung.sf_pro_pt())
        .find(|pt| size_q(*pt) == spec.size_q)
        .unwrap_or(spec.size_pt())
}

/// A font cache, measuring for a tree's layout.
///
/// The cache is borrowed for as long as the layout runs; its own
/// caches are what make a second measurement of the same string free.
pub struct FontMeasure<'a> {
    font: RefCell<&'a mut FontCache>,
}

impl<'a> FontMeasure<'a> {
    pub fn new(font: &'a mut FontCache) -> Self {
        Self { font: RefCell::new(font) }
    }
}

impl FontMeasure<'_> {
    /// The measurer the controls' geometry functions take, over the
    /// same font cache: a control sized while a tree is described is
    /// the size it is painted at.
    pub fn for_controls(&self) -> impl FnMut(&str, f64, u16) -> f64 + '_ {
        move |s, pt, weight| {
            self.font.borrow_mut().measure_ui_text_at_size(s, weight, ShapeOptions::default(), pt)
        }
    }
}

impl TextMeasure for FontMeasure<'_> {
    fn advance(&self, text: &str, font: FontSpec) -> f64 {
        // through f32, as the painter takes it
        self.font.borrow_mut().measure_ui_text_at_size(
            text,
            font.weight,
            ShapeOptions::default(),
            exact_pt(font),
        ) as f32 as f64
    }

    fn metrics(&self, font: FontSpec) -> FontMetrics {
        let pt = exact_pt(font);
        let px = crate::ui::px_per_pt();
        // the cache holds the interface font's line at its startup
        // size, in window units; the other sizes are in proportion
        let k = pt / UI_FONT_POINT * px / crate::ui::chrome_scale();
        let cache = self.font.borrow();
        FontMetrics {
            line_h: cache.ui_cell_h * k,
            ascent: cache.ui_ascent * k,
            cap_h: sf_pro_cap_height(pt) * px,
            // the room the panels leave under a last line
            descent: pt * 0.22 * px,
        }
    }

    fn centring(&self, text: &str, font: FontSpec) -> Extent {
        let (above, below) =
            crate::ui::core::ink::centring(&mut self.font.borrow_mut(), text, exact_pt(font), font.weight);
        Extent { above: above as f64, below: below as f64 }
    }

    fn fit_prefix(&self, text: &str, font: FontSpec, max_w: f64) -> Fit {
        super::fit::fit_prefix(&mut self.font.borrow_mut(), text, font, max_w)
    }

    fn mono_cells(&self, text: &str) -> usize {
        crate::ui::core::text_fit::cells(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::density_kit::at_scale;
    use crate::ui::theme::PanelText;

    fn spec_of(role: PanelText) -> FontSpec {
        FontSpec::new(role.pt(), role.weight())
    }

    #[test]
    fn the_tree_is_told_what_the_painters_work_out() {
        for scale in [1.0, 2.0] {
            at_scale(scale, || {
                let mut font = FontCache::build().expect("font");
                for role in [PanelText::Title, PanelText::Label, PanelText::Item, PanelText::Caption] {
                    for s in ["Reclaim idle panes", "日本語", "简体中文", "Wheel speed gyp"] {
                        let want_w = font.measure_ui_text_at_size(s, role.weight(), ShapeOptions::default(), role.pt())
                            as f32 as f64;
                        let want_c = crate::ui::core::ink::centring(&mut font, s, role.pt(), role.weight());
                        let m = FontMeasure::new(&mut font);
                        assert_eq!(m.advance(s, spec_of(role)), want_w, "{role:?} {s:?} at {scale}");
                        let by = m.centring(s, spec_of(role));
                        assert_eq!((by.above as f32, by.below as f32), want_c);
                        assert_eq!(by.above.fract() + by.below.fract(), 0.0, "rows are whole");
                        let fm = m.metrics(spec_of(role));
                        assert_eq!(fm.cap_h, crate::ui::theme::spec::px(role.cap()));
                        assert_eq!(fm.descent, crate::ui::theme::spec::px(role.descent()));
                        assert!(fm.ascent > fm.cap_h && fm.line_h > fm.ascent, "{fm:?}");
                    }
                }
                let m = FontMeasure::new(&mut font);
                assert_eq!(m.mono_cells("a日b"), 4);
            });
        }
    }

    #[test]
    fn a_line_doubles_with_the_display() {
        let line = |scale: f64| {
            at_scale(scale, || {
                let mut font = FontCache::build().expect("font");
                FontMeasure::new(&mut font).metrics(spec_of(PanelText::Label))
            })
        };
        let (one, two) = (line(1.0), line(2.0));
        assert!((two.line_h - 2.0 * one.line_h).abs() < 1e-9, "{one:?} {two:?}");
        assert!((two.ascent - 2.0 * one.ascent).abs() < 1e-9);
        assert_eq!(two.cap_h, 2.0 * one.cap_h);
    }

    #[test]
    fn han_is_centred_by_its_ink_and_latin_by_its_capitals() {
        let mut font = FontCache::build().expect("font");
        let m = FontMeasure::new(&mut font);
        let spec = spec_of(PanelText::Item);
        let cap = m.metrics(spec).cap_h.round();
        let latin = m.centring("Apply", spec);
        assert_eq!((latin.above, latin.below), (cap, 0.0));
        assert_eq!(m.centring("gypsy", spec), latin, "descenders do not move a Latin label");
        let han = m.centring("適用", spec);
        assert!(han.above > cap, "Han ink stands above a capital: {han:?} against {cap}");
    }
}
