//! Rectangles and alignment for layout.
//!
//! Layout works in `f64` device pixels and keeps fractions; what
//! reaches a scene is narrowed there.  Origin top-left, y down.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    /// Half-open: the far edges belong to whatever is next.
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }

    /// Whether `other` lies wholly inside, with room for the rounding
    /// a sum of fractional heights picks up.
    pub fn holds(&self, other: &Rect) -> bool {
        const EPS: f64 = 1e-9;
        other.x >= self.x - EPS
            && other.y >= self.y - EPS
            && other.right() <= self.right() + EPS
            && other.bottom() <= self.bottom() + EPS
    }

    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect { x, y, w: (r - x).max(0.0), h: (b - y).max(0.0) }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn translated(&self, dx: f64, dy: f64) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }

    /// A `w × h` box placed inside by `align`.
    pub fn place(&self, w: f64, h: f64, align: Align) -> Rect {
        let (fh, fv) = align.factors();
        Rect { x: self.x + (self.w - w) * fh, y: self.y + (self.h - h) * fv, w, h }
    }
}

/// Nine anchors.  Leading and trailing rather than left and right, so
/// a right-to-left layout is a change of what they resolve to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    TopLeading,
    Top,
    TopTrailing,
    Leading,
    Center,
    Trailing,
    BottomLeading,
    Bottom,
    BottomTrailing,
}

impl Align {
    /// Fractions of the free space to put before the box, per axis.
    pub fn factors(self) -> (f64, f64) {
        let h = match self {
            Align::TopLeading | Align::Leading | Align::BottomLeading => 0.0,
            Align::Top | Align::Center | Align::Bottom => 0.5,
            Align::TopTrailing | Align::Trailing | Align::BottomTrailing => 1.0,
        };
        let v = match self {
            Align::TopLeading | Align::Top | Align::TopTrailing => 0.0,
            Align::Leading | Align::Center | Align::Trailing => 0.5,
            Align::BottomLeading | Align::Bottom | Align::BottomTrailing => 1.0,
        };
        (h, v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_centred_in_a_band_sits_half_the_slack_down() {
        let band = Rect::new(10.0, 100.0, 200.0, 28.0);
        let r = band.place(50.0, 8.0, Align::Leading);
        assert_eq!(r, Rect::new(10.0, 110.0, 50.0, 8.0));
        let r = band.place(50.0, 8.0, Align::BottomTrailing);
        assert_eq!(r, Rect::new(160.0, 120.0, 50.0, 8.0));
    }

    #[test]
    fn holding_is_whole_containment() {
        let view = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(view.holds(&Rect::new(0.0, 90.0, 100.0, 10.0)));
        assert!(!view.holds(&Rect::new(0.0, 90.0, 100.0, 10.5)));
        assert!(!view.holds(&Rect::new(0.0, -0.5, 100.0, 10.0)));
    }
}
