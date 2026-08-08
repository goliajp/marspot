//! The five sizes a panel is allowed to set text in.
//!
//! Before this, each panel picked its own numbers.  The `Cc` panel drew
//! its two titles through `ViewPainter::ui_text` — SF Pro at the
//! startup chrome size, weight 600 — and everything else in the
//! terminal's mono cell.  The settings panel, built later, picked 17 /
//! 13 / 12 / 11 / 10.5 pt by hand.  The result was a settings panel
//! whose **row labels** were the size of every other panel's **title**,
//! which is what "字太大了" was describing: not one number too big, but
//! two panels measuring from different rulers.
//!
//! So the roles are named and the sizes derived from [`UiSize`] — the
//! cap-height ladder the dev panel already draws from.  A panel asks
//! for a *role*, never a number, and two panels showing the same kind
//! of thing come out the same size because they asked for the same
//! word.
//!
//! Mono is not replaced.  Tabular data — a process tree, a percentage
//! column — stays in the terminal cell font, where columns line up for
//! free and proportional text would only wobble.  This ladder is for
//! everything that is prose: titles, labels, the sentence under a
//! label.

use super::super::view::type_scale::UiSize;

/// What a run of panel text *is*, rather than how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelText {
    /// The panel's own name, once at the top.
    Title,
    /// A group of rows inside a panel.
    Section,
    /// The thing a row is about.
    Label,
    /// A row you *pick* — a menu item, a list entry.
    ///
    /// Same size as [`Self::Label`], lighter.  A label sits above an
    /// explanation and has to win against it; a menu item has nothing
    /// to win against, and at semibold a column of them reads as a
    /// column of headings.  Every native menu on the machine sets
    /// these at regular weight.
    Item,
    /// The sentence under a label: what it costs, what it means.
    Secondary,
    /// Footnote — a file path, a timestamp.  Same size as
    /// [`Self::Secondary`]; it is set apart by colour, because a
    /// sixth rung would be a distinction nobody can see.
    Caption,
}

impl PanelText {
    /// SF Pro point size.
    pub fn pt(self) -> f64 {
        match self {
            PanelText::Title => UiSize::Title.sf_pro_pt(),
            PanelText::Section => UiSize::Heading.sf_pro_pt(),
            PanelText::Label | PanelText::Item => UiSize::Body.sf_pro_pt(),
            PanelText::Secondary | PanelText::Caption => UiSize::Small.sf_pro_pt(),
        }
    }

    /// Weight, in the CSS 100..900 sense.
    ///
    /// The ladder leans on size first and weight second: a label at
    /// 600 against a secondary line at 400 separates them even where
    /// the size difference is small, and neither needs colour to do
    /// the work.
    pub fn weight(self) -> u16 {
        match self {
            PanelText::Title => 700,
            PanelText::Section => 600,
            PanelText::Label => 600,
            PanelText::Item => 400,
            PanelText::Secondary | PanelText::Caption => 400,
        }
    }

    /// Cap height in pt — what optical centring and line spacing
    /// measure from.  See [`crate::ui::view::type_scale`].
    pub fn cap(self) -> f64 {
        crate::ui::view::type_scale::sf_pro_cap_height(self.pt())
    }

    /// Rough descender depth in pt, for the space under a last line.
    pub fn descent(self) -> f64 {
        self.pt() * 0.22
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder has to be strictly descending, or a role means
    /// nothing: a "section" that is not bigger than a "label" is just
    /// two labels.
    #[test]
    fn the_ladder_descends_and_never_ties() {
        let ladder = [
            PanelText::Title,
            PanelText::Section,
            PanelText::Label,
            PanelText::Secondary,
        ];
        for w in ladder.windows(2) {
            assert!(
                w[0].pt() > w[1].pt(),
                "{:?} ({:.2}pt) must be bigger than {:?} ({:.2}pt)",
                w[0], w[0].pt(), w[1], w[1].pt(),
            );
        }
        // The footnote shares Secondary's size on purpose, and a menu
        // item shares Label's — both are set apart by weight or
        // colour, not by a rung nobody could see.
        assert_eq!(PanelText::Caption.pt(), PanelText::Secondary.pt());
        assert_eq!(PanelText::Item.pt(), PanelText::Label.pt());
        assert!(
            PanelText::Item.weight() < PanelText::Label.weight(),
            "a menu item must be lighter than a label, or a column of \
             them reads as a column of headings",
        );
    }

    /// A panel's row label must not come out the size of another
    /// panel's title — the exact complaint this module exists for.
    #[test]
    fn a_label_is_clearly_smaller_than_a_title() {
        assert!(
            PanelText::Label.pt() < PanelText::Title.pt() * 0.75,
            "label {:.2}pt vs title {:.2}pt is not a hierarchy",
            PanelText::Label.pt(),
            PanelText::Title.pt(),
        );
    }
}
