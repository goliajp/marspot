//! A length kept in window units.
//!
//! A window moved to a display of another density keeps its state, and
//! any of that state held in pixels is then wrong by the ratio of the
//! two: a panel dragged 120 px across a 1x display is 60 window units
//! from the middle, and on a 2x display that is 240 px.  What outlives
//! a frame -- how far a panel was dragged or scrolled, where a menu
//! was opened -- is kept in these, and turned into the pixels of the
//! display in use where it is read.

use super::chrome_scale;

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Units(f64);

impl Units {
    /// `px` physical pixels of the display in use now.
    pub fn from_px(px: f64) -> Self {
        Self(px / chrome_scale())
    }

    /// In physical pixels of the display in use now.
    pub fn px(self) -> f64 {
        self.0 * chrome_scale()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::density_kit::at_scale;

    #[test]
    fn a_length_follows_the_display_and_comes_back_exactly() {
        for px in [0.0, 1.0, 37.0, 120.5, 1e-3, 4095.0] {
            let kept = at_scale(1.0, || Units::from_px(px));
            assert_eq!(at_scale(1.0, || kept.px()), px);
            assert_eq!(at_scale(2.0, || kept.px()), px * 2.0);
            assert_eq!(at_scale(1.0, || kept.px()), px, "back from the denser display");
            assert_eq!(at_scale(2.0, || Units::from_px(px * 2.0)), kept, "the same place, taken there");
        }
        assert_eq!(Units::default(), at_scale(2.0, || Units::from_px(0.0)));
    }
}
