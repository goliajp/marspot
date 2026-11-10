//! Lengths, and the one place they become pixels.
//!
//! A tree is described in points, cells and fractions of the parent;
//! [`Metrics`] is what turns each into device pixels.  Nothing else in
//! the tree multiplies by a scale, so two callers cannot disagree about
//! what a point is.

/// What a layout pass resolves lengths against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Device pixels per point.
    pub px_per_pt: f64,
    /// Advance of one cell of the monospaced font, device pixels.
    pub cell_w: f64,
    /// Height of one row of the monospaced font.
    pub cell_h: f64,
    /// Baseline of the monospaced font below the top of its row.
    pub cell_ascent: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// Points.  Scales with the display.
    Pt(f64),
    /// Device pixels at any scale: hairlines and one-pixel borders.
    Px(f64),
    /// Fraction of the parent's extent on the same axis, `0.0..=1.0`.
    Pct(f64),
    /// Cells of the monospaced font.
    Ch(f64),
}

impl Length {
    pub const ZERO: Length = Length::Px(0.0);

    pub fn resolve(self, parent: f64, m: &Metrics) -> f64 {
        match self {
            Length::Pt(p) => p * m.px_per_pt,
            Length::Px(p) => p,
            Length::Pct(f) => parent * f,
            Length::Ch(c) => c * m.cell_w,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edges {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}

impl Edges {
    pub const ZERO: Edges = Edges::all(Length::ZERO);

    pub const fn all(l: Length) -> Self {
        Self { top: l, right: l, bottom: l, left: l }
    }

    /// Horizontal for left and right, vertical for top and bottom.
    pub const fn xy(h: Length, v: Length) -> Self {
        Self { top: v, right: h, bottom: v, left: h }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(px_per_pt: f64) -> Metrics {
        Metrics { px_per_pt, cell_w: 9.0, cell_h: 18.0, cell_ascent: 14.0 }
    }

    #[test]
    fn a_point_is_whatever_the_metrics_say_and_nothing_else() {
        for (scale, want) in [(1.0, 12.0), (1.5, 18.0), (2.0, 24.0)] {
            assert_eq!(Length::Pt(12.0).resolve(999.0, &at(scale)), want, "at {scale}");
        }
        // the cell and the parent do not enter into it
        let odd = Metrics { px_per_pt: 2.0, cell_w: 1.0, cell_h: 1.0, cell_ascent: 1.0 };
        assert_eq!(Length::Pt(12.0).resolve(0.0, &odd), 24.0);
    }

    #[test]
    fn a_device_pixel_is_one_pixel_at_every_scale() {
        for scale in [1.0, 1.5, 2.0] {
            assert_eq!(Length::Px(1.0).resolve(999.0, &at(scale)), 1.0, "at {scale}");
        }
    }

    #[test]
    fn fractions_follow_the_parent_and_cells_follow_the_cell() {
        for scale in [1.0, 1.5, 2.0] {
            assert_eq!(Length::Pct(0.25).resolve(400.0, &at(scale)), 100.0);
            assert_eq!(Length::Ch(3.0).resolve(400.0, &at(scale)), 27.0);
        }
    }

    /// The scale cases above all pass if `resolve` ignores its input;
    /// this one fails then.
    #[test]
    fn the_scale_cases_can_fail() {
        assert_ne!(
            Length::Pt(12.0).resolve(0.0, &at(1.0)),
            Length::Pt(12.0).resolve(0.0, &at(2.0)),
        );
        assert_ne!(Length::Px(1.0).resolve(0.0, &at(2.0)), Length::Pt(1.0).resolve(0.0, &at(2.0)));
    }
}
