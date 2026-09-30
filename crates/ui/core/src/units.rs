//! One ruler.
//!
//! The application this came out of had four coexisting length units —
//! logical points times a scale, points times pixels-per-point, cell
//! multiples, and raw physical pixels — and code that converted between
//! them at the call site, differently in different places.  There is
//! one here, and conversions are named.

/// A physical pixel on the display being drawn to.
///
/// Everything that reaches a backend is in these.  Logical points and
/// cell multiples are inputs to layout, never outputs of it: by the
/// time a rectangle is in a scene it has been resolved.
pub type Px = f32;

/// A rectangle in physical pixels, origin top-left, y growing down.
///
/// Top-left because that is where every 2D drawing API puts it and
/// where text starts.  The clip-space flip that some graphics APIs
/// want belongs in that backend's vertex shader, not here.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RectPx {
    pub x: Px,
    pub y: Px,
    pub w: Px,
    pub h: Px,
}

impl RectPx {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub const fn new(x: Px, y: Px, w: Px, h: Px) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> Px {
        self.x + self.w
    }
    pub fn bottom(&self) -> Px {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// The part of `self` that is also inside `other`.
    ///
    /// Empty when they do not overlap — callers test `is_empty` rather
    /// than getting an `Option`, because the common use is "clip this
    /// and carry on", and a rectangle with no area draws nothing.
    pub fn intersect(&self, other: &RectPx) -> RectPx {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        RectPx { x, y, w: (r - x).max(0.0), h: (b - y).max(0.0) }
    }

    pub fn contains(&self, x: Px, y: Px) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// Which way text and layout run.
///
/// A first-class property rather than a late flag: right-to-left is not
/// a mirrored afterthought, it is the same infrastructure that carries
/// bidirectional text, and layout has to know before it places
/// anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    LeftToRight,
    RightToLeft,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_of_disjoint_rects_has_no_area() {
        let a = RectPx::new(0.0, 0.0, 10.0, 10.0);
        let b = RectPx::new(20.0, 20.0, 5.0, 5.0);
        assert!(a.intersect(&b).is_empty());
        // and it is not negative, which would sail through a `w > 0`
        // test somewhere downstream as a huge rectangle
        assert_eq!(a.intersect(&b).w, 0.0);
        assert_eq!(a.intersect(&b).h, 0.0);
    }

    #[test]
    fn intersection_is_the_overlap() {
        let a = RectPx::new(0.0, 0.0, 10.0, 10.0);
        let b = RectPx::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), RectPx::new(5.0, 5.0, 5.0, 5.0));
    }

    #[test]
    fn containment_excludes_the_far_edge() {
        let r = RectPx::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(9.9, 9.9));
        // half-open: the pixel at x = 10 belongs to whatever is next,
        // or two adjacent rects would both claim it
        assert!(!r.contains(10.0, 5.0));
    }
}
