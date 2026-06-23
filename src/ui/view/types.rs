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

/// Scroll-wheel target id — used by `Modifier::OnScroll` to route
/// wheel events from `hit_test_scroll` to the right reducer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollWheelId(pub u32);

/// Drag-begin target id — pairs the begin event with subsequent
/// move / end events so reducers know which interaction continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DragId(pub u32);

/// Focus chain target id — `.focusable(FocusId)` adds the view to
/// the Tab navigation ring keyed by this id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FocusId(pub u32);

/// Logical key code — a small enum covering the keys we route at
/// view-tree level.  Letter / digit keys come through as their char
/// (lowercase ASCII);  function/arrow keys use named variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter, Escape, Tab, Backspace, Delete, Space,
    ArrowLeft, ArrowRight, ArrowUp, ArrowDown,
    Home, End, PageUp, PageDown,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
}

/// Keyboard shortcut binding — `.shortcut(KeyEquivalent, ActionId)`
/// fires when the user types this combination.  Routing happens at
/// the host level: the App layer collects `.shortcut()` modifiers
/// from the tree each frame, then on KeyDown matches `(key, mods)`
/// against the table to dispatch the `ActionId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyEquivalent {
    pub key: Key,
    pub mods: Modifiers,
}

impl KeyEquivalent {
    pub const fn cmd(key: Key) -> Self {
        Self { key, mods: Modifiers { command: true, shift: false, control: false, option: false } }
    }
    pub const fn cmd_shift(key: Key) -> Self {
        Self { key, mods: Modifiers { command: true, shift: true, control: false, option: false } }
    }
    pub const fn ctrl(key: Key) -> Self {
        Self { key, mods: Modifiers { command: false, shift: false, control: true, option: false } }
    }
    pub const fn plain(key: Key) -> Self {
        Self { key, mods: Modifiers { command: false, shift: false, control: false, option: false } }
    }
}

/// Pointer location in physical pixels (matches LaidOut.rect coords).
pub type Point = (f64, f64);

/// Modifier-key bitmask — same shape as the main app's input layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub option: bool,
    pub command: bool,
}

/// Unified input event surface for the View tree's gesture model.
/// Hit-test routes events to `ActionId` / `ScrollWheelId` / etc.
/// via the existing `hit_test_*` functions.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Click(Point, Modifiers),
    DoubleClick(Point, Modifiers),
    RightClick(Point, Modifiers),
    DragBegin { at: Point, mods: Modifiers, drag_id: DragId },
    DragMove  { from: Point, to: Point, drag_id: DragId },
    DragEnd   { from: Point, to: Point, drag_id: DragId },
    Hover { at: Point, entered: bool, hover_id: HoverId },
    Scroll { at: Point, delta: (f64, f64), precise: bool, target: ScrollWheelId },
}

/// In-progress drag — kept by host across move events.  Established
/// at DragBegin, mutated on DragMove, dropped on DragEnd.
#[derive(Clone, Debug)]
pub struct DragInProgress {
    pub drag_id: DragId,
    pub started_at: Point,
    pub current: Point,
    pub modifiers: Modifiers,
}

impl DragInProgress {
    pub fn delta(&self) -> (f64, f64) {
        (self.current.0 - self.started_at.0, self.current.1 - self.started_at.1)
    }
}

/// Animation curve — interpolation easing.  v1 supports linear +
/// the standard ease-in/out cubics;  spring physics curves are
/// v2+ when we ship real animation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnimCurve {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    /// Damped-spring approximation — overshoots slightly past 1.0
    /// before settling.  Cheap closed-form (real ODE-based spring
    /// physics is v2+ when frame scheduler lands).  `bounce ∈ [0,
    /// 1]` controls overshoot amplitude.
    Spring { bounce: f64 },
}

impl AnimCurve {
    /// Resolve eased fraction at `t ∈ [0.0, 1.0]`.
    pub fn ease(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            AnimCurve::Linear    => t,
            AnimCurve::EaseIn    => t * t,
            AnimCurve::EaseOut   => 1.0 - (1.0 - t) * (1.0 - t),
            AnimCurve::EaseInOut => {
                if t < 0.5 { 2.0 * t * t } else { 1.0 - 2.0 * (1.0 - t).powi(2) }
            }
            AnimCurve::Spring { bounce } => {
                // Damped-cosine settling — 1 - e^(-6t) cos(2πt × 2)
                // × bounce.  Coefficients tuned so overshoot at ~70%
                // of t lands near 1.05 for bounce=1.0 and the value
                // settles to 1.0 by t=1.0.
                let b = bounce.clamp(0.0, 1.0);
                let damp = (-6.0 * t).exp();
                let osc = (2.0 * std::f64::consts::PI * t * 2.0).cos();
                1.0 - damp * (1.0 + b * 0.3 * osc)
            }
        }
    }
}

/// Generic time-based animation from `from` to `to`.  `T: Lerp` —
/// implement for Color / Length / f64 / etc.
///
/// v1 = data type only.  Real frame scheduling(`schedule_redraw_in`
/// + per-frame interpolation)is v2+ animation rollout;  this struct
/// already gives callers the carrier shape.
#[derive(Clone, Copy, Debug)]
pub struct Anim<T: Copy> {
    pub from: T,
    pub to: T,
    pub elapsed: f64,    // ms since started
    pub duration: f64,   // ms
    pub curve: AnimCurve,
}

impl<T: Copy + Lerp> Anim<T> {
    /// Sample the animation at current elapsed time.
    pub fn current(&self) -> T {
        if self.duration <= 0.0 { return self.to; }
        let t = (self.elapsed / self.duration).clamp(0.0, 1.0);
        let t = self.curve.ease(t);
        T::lerp(self.from, self.to, t)
    }
    pub fn finished(&self) -> bool { self.elapsed >= self.duration }
}

/// Linear interpolation trait — primitive types + Color implement.
pub trait Lerp: Sized {
    fn lerp(from: Self, to: Self, t: f64) -> Self;
}
impl Lerp for f64 {
    fn lerp(a: f64, b: f64, t: f64) -> f64 { a + (b - a) * t }
}
impl Lerp for crate::ui::core::Color {
    fn lerp(a: Self, b: Self, t: f64) -> Self {
        use crate::ui::core::Color;
        Color {
            r: ((a.r as f64) * (1.0 - t) + (b.r as f64) * t) as u8,
            g: ((a.g as f64) * (1.0 - t) + (b.g as f64) * t) as u8,
            b: ((a.b as f64) * (1.0 - t) + (b.b as f64) * t) as u8,
            a: a.a * (1.0 - t) + b.a * t,
        }
    }
}

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
