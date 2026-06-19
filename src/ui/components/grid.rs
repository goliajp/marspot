//! `Grid` — thin wrapper over a row-major `[Rect]` of cells +
//! mutually-exclusive separation style.  Either:
//!
//! - **Gap mode** — cells are separated by an empty band of
//!   `width` pixels, nothing is painted between them (the
//!   surrounding View's BG shows through).  Used by cluster /
//!   card layouts where the negative space IS the divider.
//! - **Seam mode** — cells are separated by a hairline of the
//!   given `SeamStyle`.  Delegates to the existing `GridSeams`
//!   component so the implementation stays in one place.
//!
//! The two modes are mutually exclusive on purpose: drawing both
//! a visible seam AND a gap is just two layers of visual noise.
//! The grid's geometry computation (cell rect layout) is owned
//! by the caller — `Grid::paint` only handles the seam/gap
//! visual.  This keeps the abstraction non-prescriptive about
//! how cells get sized (Layout::build computes the main grid;
//! LayoutModal computes the card grid; both can feed `Grid` the
//! resulting rects).

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;
use super::grid_seams::SeamStyle;

#[derive(Debug, Clone, Copy)]
pub enum GridStyle {
    /// No visible separator.  `width` is informational — the
    /// caller already left gaps of this size between cells when
    /// computing the rects.  Paint is a no-op.
    Gap { width: f64 },
    /// Hairline between cells.  Same vertical+horizontal style.
    Seam(SeamStyle),
}

pub struct Grid<'a> {
    pub cells: &'a [Rect],
    pub cols: usize,
    pub rows: usize,
    pub style: GridStyle,
}

impl<'a> Grid<'a> {
    pub fn paint(&self, p: &mut ViewPainter) {
        if self.cols == 0 || self.rows == 0 || self.cells.is_empty() {
            return;
        }
        match self.style {
            GridStyle::Gap { width: _ } => {
                // Nothing to draw — the gap IS the visual.
            }
            GridStyle::Seam(s) => {
                use super::grid_seams::GridSeams;
                GridSeams {
                    cells: self.cells,
                    cols: self.cols,
                    rows: self.rows,
                    vertical: s,
                    horizontal: s,
                }
                .paint(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_mode_paints_nothing() {
        // No painter spy in this crate — just smoke that the
        // gap variant constructs cleanly and the data-layer
        // invariants pass.
        let cells = vec![
            Rect { x: 0.0, y_top: 0.0, w: 50.0, h: 50.0 },
            Rect { x: 60.0, y_top: 0.0, w: 50.0, h: 50.0 },
        ];
        let g = Grid {
            cells: &cells,
            cols: 2,
            rows: 1,
            style: GridStyle::Gap { width: 10.0 },
        };
        match g.style {
            GridStyle::Gap { width } => assert_eq!(width, 10.0),
            _ => unreachable!(),
        }
    }

    #[test]
    fn seam_mode_carries_style_through() {
        let style = SeamStyle { color: [1.0; 4], thickness: 2.0 };
        let g = Grid {
            cells: &[],
            cols: 0,
            rows: 0,
            style: GridStyle::Seam(style),
        };
        match g.style {
            GridStyle::Seam(s) => {
                assert_eq!(s.thickness, 2.0);
            }
            _ => unreachable!(),
        }
    }
}
