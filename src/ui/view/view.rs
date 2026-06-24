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
pub use super::types::GridTrack;

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
    /// Uniform-height virtualised list — only items in the visible
    /// viewport get laid out + painted.  See `lazy.rs` for details.
    LazyVStack {
        items: Vec<View>,
        gap: Length,
        item_height: Length,
        id: ViewId,
    },
    /// Uniform-width horizontal mirror of `LazyVStack`.
    LazyHStack {
        items: Vec<View>,
        gap: Length,
        item_width: Length,
        id: ViewId,
    },
    /// Uniform-cell grid — items flow left-to-right, top-to-bottom
    /// across `cols` columns.  All cells = `cell_w` × `cell_h`.
    /// `col_gap` between columns, `row_gap` between rows;  pass the
    /// same value for both via `grid(items, cols, cell_w, cell_h,
    /// gap)` shortcut.
    Grid {
        items: Vec<View>,
        cols: usize,
        col_gap: Length,
        row_gap: Length,
        cell_w: Length,
        cell_h: Length,
    },
    /// Variable-track grid — each column track has its own size,
    /// each row gets its own height too.  Items still flow l-to-r,
    /// t-to-b across `tracks_w.len()` columns.  Cells = `tracks_w[
    /// col] × tracks_h[row]` (cycle if rows underspecified).
    VariableGrid {
        items: Vec<View>,
        tracks_w: Vec<GridTrack>,
        tracks_h: Vec<GridTrack>,
        gap: (Length, Length),  // (col_gap, row_gap)
    },
    /// Stateful binary switch — visual capsule with circle inside.
    /// `id` keys into `HostState` for the boolean.
    Toggle {
        id: ViewId,
    },
    /// Segmented picker — horizontal bar of mutually-exclusive
    /// options.  Selection index stored under `id` in `HostState`.
    Picker {
        id: ViewId,
        options: Vec<String>,
    },
    /// Image primitive — type defined; real renderer Image
    /// primitive support is a v2+ follow-up (currently paints as
    /// a tinted placeholder rect).  Always wrap in `.frame(width:,
    /// height:)` since we have no intrinsic image dims yet.
    Image(Image),
    /// Geometric shape primitive — Circle / Capsule / RoundedRect /
    /// Path (v2+).  v1 renders as approximating rects/lines through
    /// the existing Canvas; richer SDF paths land later.
    Shape(ShapeSpec),

    // ─── Modified (modifier chain internal form) ──────────────
    Modified {
        child: Box<View>,
        mods: Vec<Modifier>,
    },
}

#[derive(Clone, Debug)]
pub struct Image {
    pub source: ImageSource,
    pub mode: ContentMode,
    pub tint: Option<Color>,
}

#[derive(Clone, Debug)]
pub enum ImageSource {
    /// Glyph atlas entry — the renderer's existing chrome SDF
    /// path can paint this.  Reserved name; wiring lands when
    /// real Image primitive is added.
    Glyph(u32),
    /// Inline RGBA bytes (PNG/JPEG decoded).  Paint emits a
    /// placeholder until Image primitive is added.
    Raw(std::sync::Arc<Vec<u8>>),
    /// IOSurface — for shared GPU images (future use).
    IOSurface(u32),
    /// Placeholder named token (eg system icon) — v1 paints as
    /// tinted rect.
    Named(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMode { Fit, Fill, Center }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeSpec {
    Circle { fill: Color },
    Capsule { fill: Color },
    RoundedRect { radius: Length, fill: Color },
}

/// Linear gradient — list of color stops along an axis.  Currently
/// used by `.background_gradient()`.  Paint emits as a series of
/// solid-color rect bands (cheap approximation until renderer adds
/// a Gradient primitive).
#[derive(Clone, Debug)]
pub struct LinearGradient {
    pub stops: Vec<(f64, Color)>,  // (offset 0..1, color)
    pub direction: GradientDir,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientDir {
    TopToBottom,
    LeftToRight,
    BottomToTop,
    RightToLeft,
}

/// Which physical font to lay this text run out against.
///
/// The layout pass asks a `FontMetricsProvider` for line-height and
/// per-string advance based on the variant — Mono uses Monaco cell
/// metrics (current behaviour, preserves all existing chrome that
/// laid itself out against cell pitch), Ui asks SF Pro via the
/// Phase 3 shape cache so weight + OT options participate in the
/// real advance calculation.
///
/// Same packed shape as `GlyphKey::size_q` / `ShapeOptions` so the
/// layout-side measure cache and the render-side shape cache key
/// against compatible buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextFontSpec {
    /// Monaco mono.  `size` selects a coarse height bucket (Caption /
    /// Body / Header / LargeHeader).  Weight is ignored (Monaco has
    /// no weight axis on macOS); we keep the field on `Text` so
    /// callers can request bold even when the resolved font won't
    /// honour it — Phase 5+ font variants will.
    Mono { size: TextSize },
    /// SF Pro proportional.  `size_q = round(pt × 4)` matches Phase
    /// 2's `GlyphKey::size_q_for`; `weight` is CSS 100..900 (Phase
    /// 5 variable font axis); `opts_bits` packs Phase 8 ShapeOptions
    /// (bit0 kerning / bit1 liga / bit2 calt / bit3 contextual) so
    /// the same string at the same size with different opts caches
    /// independently.
    Ui { size_q: u16, weight: u16, opts_bits: u8 },
}

impl Default for TextFontSpec {
    fn default() -> Self {
        Self::Mono { size: TextSize::Body }
    }
}

/// Pack a `ShapeOptions` into the 4 bits the `TextFontSpec::Ui`
/// variant carries.  Layout-side measure cache + render-side
/// shape cache key against the same bit pattern.
pub fn pack_shape_opts(opts: crate::font_shape::ShapeOptions) -> u8 {
    (opts.kerning as u8)
        | ((opts.liga as u8) << 1)
        | ((opts.calt as u8) << 2)
        | ((opts.contextual as u8) << 3)
}

/// Reverse of [`pack_shape_opts`].
pub fn unpack_shape_opts(bits: u8) -> crate::font_shape::ShapeOptions {
    crate::font_shape::ShapeOptions {
        kerning: (bits & 0b0001) != 0,
        liga: (bits & 0b0010) != 0,
        calt: (bits & 0b0100) != 0,
        contextual: (bits & 0b1000) != 0,
    }
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
    /// Which font path lays this run out.  Default
    /// `Mono { size: Body }` preserves the current Monaco cell
    /// behaviour for every existing caller; `Text::ui(pt, weight,
    /// opts)` switches the run onto the SF Pro shape + atlas path.
    pub font: TextFontSpec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
        let size = TextSize::Body;
        Self {
            content: content.into(),
            color: crate::ui::theme::color::FG,
            size,
            weight: TextWeight::Regular,
            align: TextAlign::Leading,
            lines: TextLines::Single { truncate: Truncate::End },
            font: TextFontSpec::Mono { size },
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
    /// Linear gradient background.  Painted as a sequence of
    /// solid-color bands approximating the stops along
    /// `LinearGradient::direction`.
    BackgroundGradient(LinearGradient),
    /// Material backdrop placeholder.  v1 emits a flat color
    /// (semi-opaque BG_PANEL).  Real macOS vibrancy via
    /// NSVisualEffectView is a v2+ AppKit follow-up.
    BackgroundMaterial(MaterialStyle),
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
    /// Accessibility label string.  Baked into `Decoration` for
    /// future NSAccessibility tree dispatch (v2+).
    AccessibilityLabel(String),
    /// Accessibility role / semantic category.
    AccessibilityRole(AxRole),
    /// Keyboard shortcut binding — when the user types this
    /// `KeyEquivalent` while the panel has focus, the reducer
    /// receives `ActionId`.  Routing is host-level: app collects
    /// shortcut tables each frame.
    Shortcut(super::types::KeyEquivalent, ActionId),
    /// Add to the Tab navigation ring at this `FocusId`.
    Focusable(super::types::FocusId),
    /// Mark the view as the initially-focused element.  At most
    /// one per frame should be set;  framework picks the first if
    /// multiple appear.
    AutoFocus,
    /// Fire when this view first appears in the tree (its
    /// `ViewId` enters the live set).  Lifecycle reconcile detects.
    OnAppear(ActionId),
    /// Fire when this view leaves the tree.
    OnDisappear(ActionId),
    /// Declarative enter / exit animation.  Framework drives via
    /// `LifecycleEvent::Appear/Disappear` once [A3] frame schedule
    /// lands.  Bake into Decoration so paint can apply during transit.
    Transition(super::types::Transition),
    /// Scale / rotate / translate transform applied at paint time.
    /// v1 = data only(paint applies translate via offset accumulator);
    /// rotate / scale need Metal vertex transform = v2+.
    Transform(super::types::Transform),
    /// Alpha mask using another view's shape.  v1 data only;  paint
    /// = noop until Metal stencil pipeline lands.
    Mask(Box<View>),
    /// Compositing blend mode.  v1 data only;  paint = noop.
    BlendMode(super::types::BlendMode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxRole {
    Button, Heading, ListItem, TextField, Image, StaticText,
    Group, Link, Checkbox, Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterialStyle {
    /// Standard system material — semi-opaque dark panel BG.
    Regular,
    /// Thicker — modal / popover.
    Thick,
    /// Thin — hud / tooltip.
    Thin,
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

/// Toggle switch keyed by `id` — state stored in `HostState`.
pub fn toggle(id: ViewId) -> View {
    View::Toggle { id }
}

/// Segmented picker keyed by `id`.  `options` is the label list;
/// selection index lives in `HostState::PickerState`.
pub fn picker(id: ViewId, options: Vec<impl Into<String>>) -> View {
    View::Picker { id, options: options.into_iter().map(Into::into).collect() }
}

/// Uniform grid (single gap value for both col + row).
pub fn grid(items: Vec<View>, cols: usize, cell_w: Length, cell_h: Length, gap: Length) -> View {
    View::Grid { items, cols, col_gap: gap, row_gap: gap, cell_w, cell_h }
}

/// Uniform grid with separate col_gap / row_gap.
pub fn grid_with_gaps(items: Vec<View>, cols: usize, cell_w: Length, cell_h: Length, col_gap: Length, row_gap: Length) -> View {
    View::Grid { items, cols, col_gap, row_gap, cell_w, cell_h }
}

/// Variable-track grid: each column / row track gets its own size
/// spec (Fixed / Flex / Auto).  Items flow l-to-r, t-to-b across
/// `tracks_w.len()` columns.
pub fn variable_grid(
    items: Vec<View>,
    tracks_w: Vec<GridTrack>,
    tracks_h: Vec<GridTrack>,
    col_gap: Length,
    row_gap: Length,
) -> View {
    View::VariableGrid { items, tracks_w, tracks_h, gap: (col_gap, row_gap) }
}

/// Image built from a `Named` source token.
pub fn image_named(name: &'static str, tint: Option<Color>) -> View {
    View::Image(Image {
        source: ImageSource::Named(name),
        mode: ContentMode::Fit,
        tint,
    })
}

/// Filled circle.
pub fn shape_circle(fill: Color) -> View {
    View::Shape(ShapeSpec::Circle { fill })
}

/// Filled capsule(pill — h × any-w).
pub fn shape_capsule(fill: Color) -> View {
    View::Shape(ShapeSpec::Capsule { fill })
}

/// Rounded-corner rect.
pub fn shape_rounded_rect(radius: Length, fill: Color) -> View {
    View::Shape(ShapeSpec::RoundedRect { radius, fill })
}

// ─── Composable presets (L5 — Card / Panel / Badge / Tooltip /
//     TabStrip).  Each is just a modifier-chain shortcut, not a
//     new View variant.  Migration target for ContextMenu etc.

/// Card preset — raised background + border + radius + soft shadow.
/// `child` is the content laid inside MD padding.
pub fn card(child: View) -> View {
    use crate::ui::theme::{color, space, radius, elev};
    child
        .padding(Edges::all(space::MD))
        .background(color::BG_RAISED)
        .border(Length::Pt(1.0), color::BORDER)
        .corner_radius(radius::MD)
        .shadow(elev::E1)
}

/// Panel preset — flatter than Card.  Used for sidebars / form
/// sections / supporting content blocks.
pub fn panel(child: View) -> View {
    use crate::ui::theme::{color, space, radius};
    child
        .padding(Edges::all(space::MD))
        .background(color::BG_PANEL)
        .corner_radius(radius::MD)
}

/// Pill-shaped status tag.  Color carries the semantic role
/// (token::color::SUCCESS / WARN / DANGER / ACCENT / ...).
pub fn badge(label: &str, c: Color) -> View {
    use crate::ui::theme::{color, radius, text};
    Text::new(label).style(text::CAPTION).color(color::BG).build()
        .padding(Edges::xy(Length::Pt(10.0), Length::Pt(2.0)))
        .background(c)
        .corner_radius(radius::PILL)
}

/// Tooltip / hint popup preset.  Use with hover-trigger logic at
/// the call site;  v1 just emits the visual.
pub fn tooltip(label: &str) -> View {
    use crate::ui::theme::{color, radius, text, elev};
    Text::new(label).style(text::CAPTION).color(color::FG).build()
        .padding(Edges::xy(Length::Pt(10.0), Length::Pt(6.0)))
        .background(color::BG)
        .border(Length::Pt(1.0), color::BORDER)
        .corner_radius(radius::SM)
        .shadow(elev::E2)
}

/// Context menu — vertical list of clickable items inside a Card.
/// `divider_after_idx` inserts a hairline divider after the N-th
/// item (None for no divider).  Each item carries an `ActionId`.
pub fn context_menu(items: Vec<&str>, divider_after_idx: Option<usize>, action_base: super::types::ActionId) -> View {
    use crate::ui::theme::{color, radius, text, elev};
    let mut rows: Vec<View> = Vec::with_capacity(items.len() + 1);
    for (i, label) in items.iter().enumerate() {
        let row = Text::new(*label).style(text::BODY).build()
            .padding(Edges::xy(Length::Pt(12.0), Length::Pt(6.0)))
            .frame(FrameSpec { width: Some(Length::Pct(1.0)), ..Default::default() })
            .on_click(super::types::ActionId(action_base.0.wrapping_add(i as u32)));
        rows.push(row);
        if let Some(d) = divider_after_idx {
            if i == d {
                rows.push(hairline_horiz(color::DIVIDER).frame(FrameSpec {
                    height: Some(Length::Pt(1.0)),
                    width: Some(Length::Pct(1.0)),
                    ..Default::default()
                }));
            }
        }
    }
    vstack(rows)
        .vstack_gap(Length::Pt(0.0))
        .background(color::BG_RAISED)
        .border(Length::Pt(1.0), color::BORDER)
        .corner_radius(radius::MD)
        .shadow(elev::E3)
}

/// Breadcrumb — `Home > Section > Subsection > Detail` style
/// navigation trail.  Last item is rendered emphasised (FG colour),
/// others muted.  Caret separators are unstyled `>` text.
pub fn breadcrumb(segments: Vec<&str>) -> View {
    use crate::ui::theme::{color, text};
    let last_idx = segments.len().saturating_sub(1);
    let mut items: Vec<View> = Vec::with_capacity(segments.len() * 2);
    for (i, s) in segments.iter().enumerate() {
        let color_for = if i == last_idx { color::FG } else { color::FG_MUTED };
        items.push(
            Text::new(*s).style(text::CAPTION).color(color_for).build()
        );
        if i < last_idx {
            items.push(Text::new("›").style(text::CAPTION).color(color::FG_DISABLED).build());
        }
    }
    hstack(items).hstack_gap(Length::Pt(6.0)).align_cross_center()
}

/// List-row preset — typical sidebar/table row pattern: leading
/// icon-or-empty slot, label, trailing detail.  Selected = accent
/// background;  click fires `action`.
pub fn list_row(label: &str, trailing: Option<&str>, selected: bool, action: super::types::ActionId) -> View {
    use crate::ui::theme::{color, radius, text};
    let bg = if selected { color::BG_SELECTED } else { color::BG };
    let fg = if selected { color::FG }          else { color::FG };
    let mut content = vec![
        Text::new(label).style(text::BODY).color(fg).build(),
        spacer(),
    ];
    if let Some(t) = trailing {
        content.push(
            Text::new(t).style(text::CAPTION).color(color::FG_MUTED).build()
        );
    }
    hstack(content)
        .hstack_gap(Length::Pt(8.0))
        .align_cross_center()
        .padding(Edges::xy(Length::Pt(12.0), Length::Pt(6.0)))
        .background(bg)
        .corner_radius(radius::SM)
        .frame(FrameSpec { width: Some(Length::Pct(1.0)), ..Default::default() })
        .on_click(action)
}

/// Tab strip — horizontal row of labelled tabs;  `selected` index
/// highlights one as active.  Each tab carries an `ActionId` so
/// reducers can pick up clicks.
pub fn tab_strip(labels: Vec<&str>, selected: usize, action_base: super::types::ActionId) -> View {
    use crate::ui::theme::{color, radius, text};
    let tabs: Vec<View> = labels.into_iter().enumerate().map(|(i, lab)| {
        let is_sel = i == selected;
        let bg = if is_sel { color::ACCENT } else { color::BG_PANEL };
        let fg = if is_sel { color::BG }     else { color::FG };
        Text::new(lab).style(text::BODY).color(fg).build()
            .padding(Edges::xy(Length::Pt(14.0), Length::Pt(6.0)))
            .background(bg)
            .corner_radius(radius::SM)
            .on_click(super::types::ActionId(action_base.0.wrapping_add(i as u32)))
    }).collect();
    hstack(tabs).hstack_gap(Length::Pt(4.0))
}

/// State for `View::Toggle`.  Lives in `HostState` keyed by ViewId.
#[derive(Clone, Copy, Debug, Default)]
pub struct ToggleState { pub on: bool }

/// State for `View::Picker`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PickerState { pub selected: usize }

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

    pub fn background_gradient(self, g: LinearGradient) -> Self {
        self.add_mod(Modifier::BackgroundGradient(g))
    }
    pub fn background_material(self, m: MaterialStyle) -> Self {
        self.add_mod(Modifier::BackgroundMaterial(m))
    }

    pub fn accessibility_label(self, s: impl Into<String>) -> Self {
        self.add_mod(Modifier::AccessibilityLabel(s.into()))
    }
    pub fn accessibility_role(self, r: AxRole) -> Self {
        self.add_mod(Modifier::AccessibilityRole(r))
    }
    pub fn shortcut(self, k: super::types::KeyEquivalent, a: ActionId) -> Self {
        self.add_mod(Modifier::Shortcut(k, a))
    }
    pub fn focusable(self, id: super::types::FocusId) -> Self {
        self.add_mod(Modifier::Focusable(id))
    }
    pub fn auto_focus(self) -> Self { self.add_mod(Modifier::AutoFocus) }
    pub fn on_appear(self, a: ActionId) -> Self { self.add_mod(Modifier::OnAppear(a)) }
    pub fn on_disappear(self, a: ActionId) -> Self { self.add_mod(Modifier::OnDisappear(a)) }
    pub fn transition(self, t: super::types::Transition) -> Self {
        self.add_mod(Modifier::Transition(t))
    }
    pub fn transform(self, t: super::types::Transform) -> Self {
        self.add_mod(Modifier::Transform(t))
    }
    pub fn mask(self, m: View) -> Self {
        self.add_mod(Modifier::Mask(Box::new(m)))
    }
    pub fn blend_mode(self, m: super::types::BlendMode) -> Self {
        self.add_mod(Modifier::BlendMode(m))
    }
}

// Text-specific modifiers — different from View modifiers (which
// apply layout-level transforms); these mutate the text payload.

impl Text {
    pub fn color(mut self, c: Color) -> Self { self.color = c; self }
    pub fn size(mut self, s: TextSize) -> Self {
        self.size = s;
        // Keep `font` in sync when the caller hasn't switched to a
        // Ui font — Mono variants must reflect the chosen bucket so
        // layout picks the right line-height.
        if let TextFontSpec::Mono { size } = &mut self.font {
            *size = s;
        }
        self
    }
    pub fn weight(mut self, w: TextWeight) -> Self { self.weight = w; self }
    pub fn text_align(mut self, a: TextAlign) -> Self { self.align = a; self }
    pub fn lines(mut self, l: TextLines) -> Self { self.lines = l; self }

    /// Switch this run to the SF Pro proportional shape path (Phase
    /// 3 + 4 + 5 + 7 + 8).  `size_pt` is logical points (matches
    /// `Length::Pt` semantics); `weight` is CSS 100..900 (Phase 5
    /// variable axis); `opts` is the Phase 8 OT toggle set
    /// (`ShapeOptions::full()` for normal chrome, `::code()` for
    /// code blocks, `::all_off()` to demo ligature-off).  The
    /// layout pass asks the `FontMetricsProvider` for real per-
    /// string advance using these fields instead of monospaced
    /// cell math.
    pub fn ui(
        mut self,
        size_pt: f64,
        weight: u16,
        opts: crate::font_shape::ShapeOptions,
    ) -> Self {
        self.font = TextFontSpec::Ui {
            size_q: crate::glyph_atlas::GlyphKey::size_q_for(size_pt),
            weight,
            opts_bits: pack_shape_opts(opts),
        };
        self
    }

    /// Apply a `TextStyle` preset — sets size + weight + color in
    /// one go.  Subsequent `.color()` / `.size()` calls override.
    /// `.style()` does NOT change `font`; the caller chains `.ui(...)`
    /// after `.style(...)` when they want SF Pro.
    pub fn style(mut self, s: TextStyle) -> Self {
        self.size = s.size;
        self.weight = s.weight;
        self.color = s.color;
        // Keep Mono font's bucket in sync (so the layout pass
        // measures against the bucket the style picked).
        if let TextFontSpec::Mono { size } = &mut self.font {
            *size = s.size;
        }
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
