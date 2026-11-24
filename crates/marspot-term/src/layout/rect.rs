//! The rectangles a layout is made of, in physical pixels.

/// A simple physical-pixel rectangle.  Used for non-cell UI chrome
/// elements (layout-picker button, picker option icons, sidebar
/// close-buttons, sidebar add-button).  Distinct from `CellRect`
/// because those carry terminal cols/rows; these are pure UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y_top: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y_top && py < self.y_top + self.h
    }
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y_top: 0.0,
        w: 0.0,
        h: 0.0,
    };

    /// F3+3.4 — uniform inset on all four sides.  `padding > 0`
    /// shrinks the rect inward; negative input clamps to 0.  When
    /// padding > half the rect's smaller axis, returns a zero-size
    /// rect at the rect's center so callers can no-op-paint
    /// without panicking on negative w/h.
    pub fn inset(self, padding: f64) -> Rect {
        let p = padding.max(0.0);
        let w = (self.w - 2.0 * p).max(0.0);
        let h = (self.h - 2.0 * p).max(0.0);
        // If padding ate the whole rect, recentre to a zero-size box
        // at the rect's centre so `place` still returns a sensible point.
        if w == 0.0 || h == 0.0 {
            return Rect {
                x: self.x + self.w * 0.5,
                y_top: self.y_top + self.h * 0.5,
                w: 0.0,
                h: 0.0,
            };
        }
        Rect {
            x: self.x + p,
            y_top: self.y_top + p,
            w,
            h,
        }
    }

    /// F3+3.4 — anchor a `(w, h)`-sized child inside this rect at
    /// the given alignment.  Resulting rect is clamped to fit
    /// inside `self` (if child larger than self, child overflows
    /// from the anchor edge / center, NOT clipped).  Pure
    /// positioning helper — caller still has to draw the box.
    pub fn place(self, w: f64, h: f64, align: Alignment) -> Rect {
        let (hx, vy) = align.factors();
        let x = self.x + (self.w - w) * hx;
        let y_top = self.y_top + (self.h - h) * vy;
        Rect { x, y_top, w, h }
    }
}

/// F3+3.4 — nine-anchor alignment, the standard React Native /
/// CSS box model knob.  Used by `Rect::place` to position a child
/// box inside a parent rect, and by `ViewPainter::text_in` to
/// position a text run inside a chrome label area.
///
/// Naming follows the CSS convention: vertical first, horizontal
/// second.  Variants:
///
/// | TopLeft     | TopCenter    | TopRight     |
/// | CenterLeft  | Center       | CenterRight  |
/// | BottomLeft  | BottomCenter | BottomRight  |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Alignment {
    /// (horizontal, vertical) fractions, each ∈ {0.0, 0.5, 1.0}.
    /// Returned tuple is `(hx, vy)` where 0 = top/left, 0.5 =
    /// center, 1.0 = bottom/right.  `Rect::place` multiplies these
    /// against `(parent.w - child.w, parent.h - child.h)` to
    /// compute the placement offset.
    pub fn factors(self) -> (f64, f64) {
        let hx = match self {
            Self::TopLeft | Self::CenterLeft | Self::BottomLeft => 0.0,
            Self::TopCenter | Self::Center | Self::BottomCenter => 0.5,
            Self::TopRight | Self::CenterRight | Self::BottomRight => 1.0,
        };
        let vy = match self {
            Self::TopLeft | Self::TopCenter | Self::TopRight => 0.0,
            Self::CenterLeft | Self::Center | Self::CenterRight => 0.5,
            Self::BottomLeft | Self::BottomCenter | Self::BottomRight => 1.0,
        };
        (hx, vy)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellRect {
    /// Physical-pixel x of the rect's left edge.
    pub x: f64,
    /// Physical-pixel y of the rect's TOP edge (y-down).  The renderer
    /// flips to y-up when computing baselines.
    pub y_top: f64,
    pub w: f64,
    pub h: f64,
    /// Terminal column count that fits in this rect at the configured
    /// font cell width.
    pub cols: u16,
    /// Terminal row count that fits, less `strip_rows`.
    pub rows: u16,
    /// Rows given up at the bottom of the cells, under the terminal's.
    /// Set by `Layout::with_strips`.
    pub strip_rows: u16,
    /// Where the terminal's part of the rect ends: its bottom edge, or
    /// the top of the rows given up.
    pub term_bottom: f64,
}
