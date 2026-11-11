//! The application's side of the interface tree.
//!
//! The tree (`golia_ui_core`) knows nothing about fonts, themes or
//! instance buffers.  What it needs from the application is here:
//!
//! - [`metrics`]: what a point and a cell are on the display in use,
//!   the same numbers `ui::theme::spec::px` and the terminal use;
//! - [`FontMeasure`]: text measured by the font cache every painter
//!   measures with;
//! - [`Tone`]: the colour roles a node can name, resolved against the
//!   theme in force when the node is painted;
//! - [`TreeOut`]: a painted tree written into the instance buffers
//!   the panels' own painters write;
//! - [`preset`]: the shared controls as nodes, sized and painted by
//!   the control's own functions.

mod fit;
pub mod measure;
pub mod out;
pub mod preset;
pub mod tone;

#[cfg(test)]
mod row_replica_tests;
#[cfg(test)]
mod text_parity_tests;

pub use measure::{FontMeasure, exact_pt};
pub use out::TreeOut;
pub use tone::Tone;

use crate::font_cache::FontCache;
use crate::ui::theme::PanelText;
use golia_ui_core::{FontSpec, Metrics, TextFont};

/// What a tree's lengths resolve against on the display in use.
///
/// A point here is the point of `ui::theme::spec`: both read
/// `ui::px_per_pt`, so a length written `Length::Pt(n)` in a tree and
/// one written `spec::px(n)` in a painter are the same pixels at every
/// scale.
pub fn metrics(font: &FontCache) -> Metrics {
    Metrics {
        px_per_pt: crate::ui::px_per_pt(),
        cell_w: font.cell_w,
        cell_h: font.cell_h,
        cell_ascent: font.ascent,
    }
}

/// The font a panel text role is set in.
pub fn font_of(role: PanelText) -> TextFont {
    TextFont::Ui(FontSpec::new(role.pt(), role.weight()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::density_kit::at_scale;
    use crate::ui::theme::spec;
    use golia_ui_core::Length;

    fn at(scale: f64) -> Metrics {
        Metrics { px_per_pt: crate::ui::px_per_pt(), cell_w: 9.0 * scale, cell_h: 18.0 * scale, cell_ascent: 14.0 * scale }
    }

    #[test]
    fn a_point_in_a_tree_is_a_point_of_the_spec_at_every_scale() {
        let lengths = [
            1.0,
            spec::space::S2,
            spec::space::S12,
            spec::card::ROW_PAD_Y,
            spec::control::SEGMENT_H,
            spec::control::SEGMENT_RADIUS,
            PanelText::Label.cap(),
            440.0,
        ];
        let mut seen = Vec::new();
        for scale in [1.0, 1.5, 2.0] {
            at_scale(scale, || {
                let m = at(scale);
                for pt in lengths {
                    assert_eq!(Length::Pt(pt).resolve(0.0, &m), spec::px(pt), "{pt} pt at scale {scale}");
                }
                // a hairline is one pixel whatever the scale
                assert_eq!(Length::Px(spec::card::SEPARATOR_PX).resolve(0.0, &m), 1.0);
                assert_eq!(Length::Px(spec::card::BORDER_PX as f64).resolve(0.0, &m), 1.0);
                seen.push(Length::Pt(1.0).resolve(0.0, &m));
            });
        }
        assert_eq!(seen, [2.0, 3.0, 4.0], "and the point follows the display");
    }

    #[test]
    fn every_rung_of_the_type_ladder_keeps_its_own_size_through_a_font_spec() {
        use crate::ui::theme::type_scale::UiSize;
        let rungs = [UiSize::Mini, UiSize::Small, UiSize::Body, UiSize::Heading, UiSize::Title, UiSize::Display];
        let mut quarters: Vec<u16> = rungs.iter().map(|r| FontSpec::new(r.sf_pro_pt(), 400).size_q).collect();
        for r in rungs {
            let spec = FontSpec::new(r.sf_pro_pt(), 600);
            assert_eq!(exact_pt(spec), r.sf_pro_pt(), "{r:?}");
            assert_ne!(spec.size_pt(), r.sf_pro_pt(), "{r:?} is not on a quarter point, or this test shows nothing");
        }
        quarters.dedup();
        assert_eq!(quarters.len(), rungs.len(), "two rungs share a quarter point");
        // off the ladder a spec is the quarter point it says
        assert_eq!(exact_pt(FontSpec::new(56.0, 400)), 56.0);
    }

    #[test]
    fn a_role_is_set_in_its_own_size_and_weight() {
        for role in [
            PanelText::Title,
            PanelText::Section,
            PanelText::Label,
            PanelText::Item,
            PanelText::Secondary,
            PanelText::Caption,
        ] {
            let TextFont::Ui(spec) = font_of(role) else { panic!("a role is interface text") };
            assert_eq!((exact_pt(spec), spec.weight), (role.pt(), role.weight()), "{role:?}");
        }
    }
}
