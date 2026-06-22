//! Layout / modifier type primitives.
//!
//! See `docs/ui-system-model.md` §5.4 for the canonical defs.

use crate::ui::core::{Color, Length};

/// Edge insets — independent values per side.  Used for `Padding`.
/// CSS `padding: top right bottom left`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edges {
    pub top: Length,
    pub right: Length,
    pub bottom: Length,
    pub left: Length,
}

impl Edges {
    pub const ZERO: Edges = Edges {
        top:    Length::Pt(0.0),
        right:  Length::Pt(0.0),
        bottom: Length::Pt(0.0),
        left:   Length::Pt(0.0),
    };

    pub const fn all(l: Length) -> Self {
        Self { top: l, right: l, bottom: l, left: l }
    }

    /// `Edges::xy(horizontal, vertical)` — `horizontal` applies to
    /// left + right, `vertical` to top + bottom.  Mirrors CSS's
    /// 2-value padding shorthand.
    pub const fn xy(h: Length, v: Length) -> Self {
        Self { top: v, right: h, bottom: v, left: h }
    }

    pub const fn horiz(l: Length) -> Self {
        Self { top: Length::Pt(0.0), right: l, bottom: Length::Pt(0.0), left: l }
    }

    pub const fn vert(l: Length) -> Self {
        Self { top: l, right: Length::Pt(0.0), bottom: l, left: Length::Pt(0.0) }
    }

    pub const fn only(
        top: Length, right: Length, bottom: Length, left: Length,
    ) -> Self {
        Self { top, right, bottom, left }
    }
}

/// Alignment within a parent / frame.  9 anchors matching the
/// SwiftUI `Alignment` shape (Leading/Trailing for I18N readiness;
/// v1 LTR, so Leading == Left, Trailing == Right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    TopLeading,    Top,    TopTrailing,
    Leading,       Center, Trailing,
    BottomLeading, Bottom, BottomTrailing,
}

impl Anchor {
    /// (h, v) ∈ [(0|0.5|1), (0|0.5|1)] — fraction of (parent - self)
    /// to offset the self origin by.  Same convention as `Rect::place`.
    pub fn factors(self) -> (f64, f64) {
        let h = match self {
            Anchor::TopLeading | Anchor::Leading | Anchor::BottomLeading => 0.0,
            Anchor::Top | Anchor::Center | Anchor::Bottom => 0.5,
            Anchor::TopTrailing | Anchor::Trailing | Anchor::BottomTrailing => 1.0,
        };
        let v = match self {
            Anchor::TopLeading | Anchor::Top | Anchor::TopTrailing => 0.0,
            Anchor::Leading | Anchor::Center | Anchor::Trailing => 0.5,
            Anchor::BottomLeading | Anchor::Bottom | Anchor::BottomTrailing => 1.0,
        };
        (h, v)
    }
}

/// Cross-axis alignment for VStack (x) / HStack (y).  Maps to CSS
/// `align-items`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlignCross {
    Start,     // CSS flex-start
    Center,    // CSS center
    End,       // CSS flex-end
    Stretch,   // CSS stretch — child fills cross axis
}

/// Main-axis distribution for VStack (y) / HStack (x).  Maps to CSS
/// `justify-content`.  No `space-evenly` in v1 (rarely used,
/// re-derivable from spaced / between).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Distribute {
    Start,    // pack to start, leftover at end
    Center,   // pack centered
    End,      // pack to end
    Spaced,   // CSS space-around — gap before + between + after
    Between,  // CSS space-between — gap between only
}

/// Frame spec — `.frame(width:, height:, min:, max:, aspect:, align:)`
/// modifier payload.  Mirrors SwiftUI `View.frame(...)` knobs.
///
/// `width: None` = hug content; `width: Some(L)` = exact (overriding
/// child's intrinsic size).  Same for height.
#[derive(Clone, Copy, Debug)]
pub struct FrameSpec {
    pub width:  Option<Length>,
    pub height: Option<Length>,
    pub min_w:  Option<Length>,
    pub max_w:  Option<Length>,
    pub min_h:  Option<Length>,
    pub max_h:  Option<Length>,
    pub aspect: Option<f64>,   // width / height
    pub align:  Anchor,        // child position inside the frame
}

impl Default for FrameSpec {
    fn default() -> Self {
        Self {
            width: None, height: None,
            min_w: None, max_w: None,
            min_h: None, max_h: None,
            aspect: None,
            align: Anchor::TopLeading,
        }
    }
}

/// Shadow spec for the `.shadow(...)` modifier.
#[derive(Clone, Copy, Debug)]
pub struct Shadow {
    pub blur: Length,
    pub offset: (Length, Length),
    pub color: Color,
}

/// Opaque, comparable id for a clickable view.  Apps own the enum;
/// `OnClick(action_id)` carries the variant index — keep < 2^32.
/// Modeling as a `u32` (not `usize`) so the wire layer can serialize
/// it portably for future event-log replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActionId(pub u32);

/// Hover region id — same shape as ActionId but separate type so
/// hover and click registries don't accidentally cross-talk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HoverId(pub u32);

/// Stable identity for a view across rebuilds.  Reserves a slot for
/// future stateful views (text input cursor, scroll position).  v1
/// host has no per-view state map, but the modifier is wired so
/// callers can already attach ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_all_equal_sides() {
        let e = Edges::all(Length::Pt(8.0));
        assert_eq!(e.top, Length::Pt(8.0));
        assert_eq!(e.left, Length::Pt(8.0));
    }

    #[test]
    fn edges_xy_splits_correctly() {
        let e = Edges::xy(Length::Pt(12.0), Length::Pt(6.0));
        assert_eq!(e.left, Length::Pt(12.0));
        assert_eq!(e.right, Length::Pt(12.0));
        assert_eq!(e.top, Length::Pt(6.0));
        assert_eq!(e.bottom, Length::Pt(6.0));
    }

    #[test]
    fn anchor_factors_match_swift_ui_semantics() {
        assert_eq!(Anchor::TopLeading.factors(), (0.0, 0.0));
        assert_eq!(Anchor::Center.factors(), (0.5, 0.5));
        assert_eq!(Anchor::BottomTrailing.factors(), (1.0, 1.0));
        assert_eq!(Anchor::TopTrailing.factors(), (1.0, 0.0));
    }

    #[test]
    fn frame_spec_default_is_hug_top_leading() {
        let f = FrameSpec::default();
        assert!(f.width.is_none());
        assert!(f.height.is_none());
        assert_eq!(f.align, Anchor::TopLeading);
    }
}
