//! `View` enum — the declarative tree node.
//!
//! See `docs/ui-system-model.md` §5.1.  immediate-mode: every
//! frame builds a fresh tree; nothing is retained between frames
//! (compared to SwiftUI / Compose).  Stable identity attaches via
//! `Modifier::Id(ViewId)` for the few views that need state
//! continuity (none in v1 — modifier is forward-compat reserve).

use crate::ui::core::{Color, Length};
use super::types::{
    AlignCross, Distribute, Edges, FrameSpec, Shadow,
    ActionId, HoverId, ViewId,
};

/// One node in the view tree.  Authors usually don't construct this
/// directly — they use builder methods (`Text::new(...)`, `vstack(...)`)
/// that produce View instances + a modifier chain (`.padding(...)`,
/// `.frame(...)`).
#[derive(Clone, Debug)]
pub enum View {
    // ─── Atoms ─────────────────────────────────────────────────
    /// Single-run text.  See `Text` below for the modifier knobs.
    Text(Text),
    /// Flex spacer — takes remaining main-axis space in a
    /// VStack / HStack, proportional to `flex`.
    Spacer { flex: u32 },
    /// Solid-color rect, hugs full constraints by default.
    Filled { color: Color, radius: Length },
    /// 1pt hairline along its long axis.  At least one of (w, h)
    /// should be small for this to read.
    Hairline { color: Color, vertical: bool },

    // ─── Containers ────────────────────────────────────────────
    /// Vertical stack — main axis y, cross axis x.
    VStack {
        children: Vec<View>,
        gap: Length,
        align: AlignCross,
        distribute: Distribute,
    },
    /// Horizontal stack — main axis x, cross axis y.
    HStack {
        children: Vec<View>,
        gap: Length,
        align: AlignCross,
        distribute: Distribute,
    },
    /// Z stack — children overlap at their own (offset, align).
    ZStack {
        children: Vec<View>,
        align: super::types::Anchor,
    },
    /// Scroll viewport — clips + offsets child along main axis.
    /// Stateful: holds offset_y in `super::scroll::SCROLL_STATES`
    /// keyed by `id`.  v1 vertical-only.
    ScrollView {
        child: Box<View>,
        id: ViewId,
    },

    // ─── Modified (modifier chain internal form) ──────────────
    Modified {
        child: Box<View>,
        mods: Vec<Modifier>,
    },
}

/// Text view payload.
#[derive(Clone, Debug)]
pub struct Text {
    pub content: String,
    pub color: Color,
    pub size: TextSize,
    pub weight: TextWeight,
    pub align: TextAlign,
    pub lines: TextLines,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSize { Caption, Body, Header, LargeHeader }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextWeight {
    Regular,
    /// Mapped to no-op in the current paint pass — chrome font has
    /// no bold cut.  Reserved for v2+ when real font weights land.
    Bold,
}

/// Bundle of `size` + `weight` + `color` used by `Text::style()`.
/// Tokens in `crate::ui::theme::text::*` are typed `TextStyle`
/// constants — components reach for them by name rather than
/// composing primitives ad-hoc.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: TextSize,
    pub weight: TextWeight,
    pub color: crate::ui::core::Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign { Leading, Center, Trailing }

#[derive(Clone, Copy, Debug)]
pub enum TextLines {
    Single { truncate: Truncate },
    Wrap { max: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncate { End, Middle, None }

impl Text {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            color: crate::ui::theme::color::FG,
            size: TextSize::Body,
            weight: TextWeight::Regular,
            align: TextAlign::Leading,
            lines: TextLines::Single { truncate: Truncate::End },
        }
    }
}

/// Clip shape — `.clip()` modifier's payload.  Rect = self bounding
/// box;  RoundedRect = self bbox with corner radius;  more shapes
/// (Circle / Capsule / Path) follow in P3 follow-up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipShape {
    Rect,
    RoundedRect(Length),
}

/// Modifier chain entry.  Order matters: outer modifiers wrap
/// inner ones (`.padding().background()` paints background outside
/// the padding box; `.background().padding()` paints background
/// inside).
#[derive(Clone, Debug)]
pub enum Modifier {
    Padding(Edges),
    Background(Color),
    Border(Length, Color),
    CornerRadius(Length),
    Shadow(Shadow),
    Frame(FrameSpec),
    Offset(Length, Length),
    /// Multiplies the cumulative alpha applied to self + descendants'
    /// painted primitives.  Clamped to [0.0, 1.0].
    Opacity(f64),
    /// Restricts paint of descendants to within self's rect (or
    /// rounded-rect for `RoundedRect`).  v1 uses culling (skips
    /// fully-outside primitives) — true pixel clip via Metal scissor
    /// stays in P3 follow-up.
    Clip(ClipShape),
    /// Aspect ratio enforcement applied in layout.  Equivalent to
    /// `.frame(aspect: Some(r))` but standalone.
    AspectRatio(f64, AspectMode),
    /// Composited on top of same-z siblings — only meaningful inside
    /// a ZStack; ignored elsewhere.
    ZIndex(i32),
    /// Don't paint self or descendants; still occupies layout space.
    Hidden(bool),
    /// Skip the whole subtree from layout AND paint.  Like CSS
    /// `display: none`.
    Collapsed(bool),
    OnHover(HoverId),
    OnClick(ActionId),
    /// Double-click target.  Paired with `OnClick` it fires both
    /// (single click + double click separately).
    OnDoubleClick(ActionId),
    /// Right-click target (already used by marspot ContextMenu).
    OnRightClick(ActionId),
    /// Scroll-wheel target — applied when pointer is inside self's
    /// rect.  `ScrollId` namespaces the target so reducers can
    /// distinguish between multiple scrollable regions.
    OnScroll(super::types::ScrollWheelId),
    /// Drag-begin target.  Reducer dispatches matching `OnDragMove`
    /// / `OnDragEnd` events keyed by `DragId` while the drag is in
    /// progress.
    OnDragBegin(super::types::DragId),
    Id(ViewId),
}

/// Aspect ratio enforcement mode.  Mirrors SwiftUI `ContentMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspectMode {
    /// Fit within the constraint (shrink to satisfy aspect, leaving
    /// gap on the other axis).  Default.
    Fit,
    /// Fill the constraint (grow on the other axis, may overflow).
    Fill,
}

// ─── Builder helpers ──────────────────────────────────────────
//
// Free functions over `View` so callers write `vstack(...)` rather
// than `View::VStack { ... }`.  Idiomatic SwiftUI surface.

pub fn vstack(children: Vec<View>) -> View {
    View::VStack {
        children,
        gap: Length::Pt(0.0),
        align: AlignCross::Start,
        distribute: Distribute::Start,
    }
}

pub fn hstack(children: Vec<View>) -> View {
    View::HStack {
        children,
        gap: Length::Pt(0.0),
        align: AlignCross::Start,
        distribute: Distribute::Start,
    }
}

pub fn zstack(children: Vec<View>) -> View {
    View::ZStack {
        children,
        align: super::types::Anchor::TopLeading,
    }
}

pub fn spacer() -> View {
    View::Spacer { flex: 1 }
}

pub fn filled(color: Color) -> View {
    View::Filled { color, radius: Length::Pt(0.0) }
}

pub fn hairline_horiz(color: Color) -> View {
    View::Hairline { color, vertical: false }
}

pub fn hairline_vert(color: Color) -> View {
    View::Hairline { color, vertical: true }
}

/// Wrap `child` in a scrolling viewport keyed by `id`.
pub fn scroll_view(id: ViewId, child: View) -> View {
    View::ScrollView { child: Box::new(child), id }
}

// ─── Modifier chain (extension methods) ───────────────────────
//
// Inherent methods on View so callers chain `view.padding(...).
// border(...)` without re-importing each modifier.

impl View {
    fn modified(self) -> Self {
        match self {
            View::Modified { .. } => self,
            other => View::Modified { child: Box::new(other), mods: Vec::new() },
        }
    }

    fn add_mod(self, m: Modifier) -> Self {
        match self.modified() {
            View::Modified { child, mut mods } => {
                mods.push(m);
                View::Modified { child, mods }
            }
            _ => unreachable!(),
        }
    }

    /// Stack-only fluent: set gap.  No-op on non-stack variants
    /// (so caller doesn't have to type-check downstream).
    pub fn vstack_gap(self, g: Length) -> Self {
        match self {
            View::VStack { children, align, distribute, .. } =>
                View::VStack { children, gap: g, align, distribute },
            View::HStack { children, align, distribute, .. } =>
                View::HStack { children, gap: g, align, distribute },
            other => other,
        }
    }
    pub fn hstack_gap(self, g: Length) -> Self { self.vstack_gap(g) }

    pub fn align_cross_start(self) -> Self    { self.align_cross(AlignCross::Start) }
    pub fn align_cross_center(self) -> Self   { self.align_cross(AlignCross::Center) }
    pub fn align_cross_end(self) -> Self      { self.align_cross(AlignCross::End) }
    pub fn align_cross_stretch(self) -> Self  { self.align_cross(AlignCross::Stretch) }

    pub fn align_cross(self, a: AlignCross) -> Self {
        match self {
            View::VStack { children, gap, distribute, .. } =>
                View::VStack { children, gap, align: a, distribute },
            View::HStack { children, gap, distribute, .. } =>
                View::HStack { children, gap, align: a, distribute },
            other => other,
        }
    }

    pub fn distribute(self, d: Distribute) -> Self {
        match self {
            View::VStack { children, gap, align, .. } =>
                View::VStack { children, gap, align, distribute: d },
            View::HStack { children, gap, align, .. } =>
                View::HStack { children, gap, align, distribute: d },
            other => other,
        }
    }

    pub fn padding(self, e: Edges) -> Self { self.add_mod(Modifier::Padding(e)) }
    pub fn background(self, c: Color) -> Self { self.add_mod(Modifier::Background(c)) }
    pub fn border(self, w: Length, c: Color) -> Self { self.add_mod(Modifier::Border(w, c)) }
    pub fn corner_radius(self, r: Length) -> Self { self.add_mod(Modifier::CornerRadius(r)) }
    pub fn shadow(self, s: Shadow) -> Self { self.add_mod(Modifier::Shadow(s)) }
    pub fn frame(self, f: FrameSpec) -> Self { self.add_mod(Modifier::Frame(f)) }
    pub fn offset(self, x: Length, y: Length) -> Self { self.add_mod(Modifier::Offset(x, y)) }
    pub fn z_index(self, z: i32) -> Self { self.add_mod(Modifier::ZIndex(z)) }
    pub fn hidden(self, h: bool) -> Self { self.add_mod(Modifier::Hidden(h)) }
    pub fn on_hover(self, id: HoverId) -> Self { self.add_mod(Modifier::OnHover(id)) }
    pub fn on_click(self, id: ActionId) -> Self { self.add_mod(Modifier::OnClick(id)) }
    pub fn on_double_click(self, id: ActionId) -> Self { self.add_mod(Modifier::OnDoubleClick(id)) }
    pub fn on_right_click(self, id: ActionId) -> Self { self.add_mod(Modifier::OnRightClick(id)) }
    pub fn on_scroll(self, id: super::types::ScrollWheelId) -> Self { self.add_mod(Modifier::OnScroll(id)) }
    pub fn on_drag_begin(self, id: super::types::DragId) -> Self { self.add_mod(Modifier::OnDragBegin(id)) }
    pub fn id(self, id: ViewId) -> Self { self.add_mod(Modifier::Id(id)) }

    pub fn opacity(self, o: f64) -> Self { self.add_mod(Modifier::Opacity(o)) }
    pub fn clip(self, shape: ClipShape) -> Self { self.add_mod(Modifier::Clip(shape)) }
    pub fn aspect_ratio(self, ratio: f64, mode: AspectMode) -> Self {
        self.add_mod(Modifier::AspectRatio(ratio, mode))
    }
    pub fn collapsed(self, c: bool) -> Self { self.add_mod(Modifier::Collapsed(c)) }
}

// Text-specific modifiers — different from View modifiers (which
// apply layout-level transforms); these mutate the text payload.

impl Text {
    pub fn color(mut self, c: Color) -> Self { self.color = c; self }
    pub fn size(mut self, s: TextSize) -> Self { self.size = s; self }
    pub fn weight(mut self, w: TextWeight) -> Self { self.weight = w; self }
    pub fn text_align(mut self, a: TextAlign) -> Self { self.align = a; self }
    pub fn lines(mut self, l: TextLines) -> Self { self.lines = l; self }

    /// Apply a `TextStyle` preset — sets size + weight + color in
    /// one go.  Subsequent `.color()` / `.size()` calls override.
    pub fn style(mut self, s: TextStyle) -> Self {
        self.size = s.size;
        self.weight = s.weight;
        self.color = s.color;
        self
    }

    /// Convenience — single-line, truncate end.
    pub fn truncate(mut self, t: Truncate) -> Self {
        self.lines = TextLines::Single { truncate: t };
        self
    }

    /// Convenience — wrap up to N lines.
    pub fn wrap(mut self, max: u32) -> Self {
        self.lines = TextLines::Wrap { max };
        self
    }

    /// Finalise as a `View` for inclusion in a tree.
    pub fn build(self) -> View {
        View::Text(self)
    }
}

impl From<Text> for View {
    fn from(t: Text) -> View { View::Text(t) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::color;

    #[test]
    fn text_new_defaults_match_token_fg() {
        let t = Text::new("hi");
        assert_eq!(t.color, color::FG);
        assert_eq!(t.size, TextSize::Body);
    }

    #[test]
    fn modifier_chain_accumulates() {
        let v = Text::new("hi").build()
            .padding(Edges::all(Length::Pt(8.0)))
            .background(color::BG_RAISED)
            .border(Length::Pt(1.0), color::BORDER);
        match v {
            View::Modified { mods, .. } => assert_eq!(mods.len(), 3),
            _ => panic!("expected modified view"),
        }
    }

    #[test]
    fn vstack_default_alignment_is_start() {
        let v = vstack(vec![Text::new("a").build()]);
        match v {
            View::VStack { align, distribute, .. } => {
                assert_eq!(align, AlignCross::Start);
                assert_eq!(distribute, Distribute::Start);
            }
            _ => panic!(),
        }
    }
}
