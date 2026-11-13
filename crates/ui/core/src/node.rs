//! One node of the tree: plain data, fixed size, no heap.
//!
//! A node owns nothing.  Its children are a linked run of indices into
//! the same array, its text is a range in the tree's one buffer (or a
//! `'static` string), and what used to be a chain of modifiers is the
//! [`Style`] every node carries.  That is what lets a rebuild clear two
//! vectors and write the tree again without touching the allocator.

use crate::action::{Action, NO_ITEM, WidgetId};
use crate::color::Rgba8;
use crate::geom::Align;
use crate::metrics::{Edges, Length};
use crate::text::TextFont;

/// Index of a node in its tree.  Not an identity: it changes with
/// every rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId(pub u32);

pub(crate) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VStack,
    HStack,
    /// Children on top of one another, later ones above.
    ZStack,
    /// A column whose height is unbounded and whose offset is the
    /// surface's scroll slot named in `Node::scroll_slot`.
    Scroll,
    /// A run of text in `Node::font`.
    Text,
    /// Takes a share of a stack's leftover main-axis space.
    Spacer,
    /// No children and no intrinsic size: its size is its style's
    /// width and height.  What a preset component is in the tree.
    Leaf,
    /// A line across a stack's cross axis that takes no room in it: a
    /// separator drawn over the edge between two children.
    Rule,
    /// A column that floats over the surface: it takes no room where
    /// it is described, stands at `Node::float_at` from the surface's
    /// corner wherever that is, inside the surface or out of it, and
    /// is drawn after everything else and found before it.  A menu, a
    /// card being dragged.
    Float,
}

/// Where a child goes on a stack's cross axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlignCross {
    #[default]
    Start,
    Center,
    End,
    /// Every child is given the stack's full cross extent.
    Stretch,
}

/// What a stack does with main-axis space its children left over.
/// Ignored when a spacer or a growing child takes that space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Distribute {
    #[default]
    Start,
    Center,
    End,
    /// Equal gaps before, between and after.
    Spaced,
    /// Equal gaps between only.
    Between,
}

/// A colour, or the name of one.
///
/// A role is resolved by whatever receives the paint, against the
/// theme in force; the tree only carries the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    Role(u8),
    Rgba(Rgba8),
}

/// How the items of a group answer the arrow keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupKind {
    #[default]
    None,
    /// Left and right move between items, and moving chooses: the
    /// item's action is emitted as the focus arrives on it.
    Segmented,
    /// Up, down, Home and End move; Space or Enter emits.
    List,
}

/// Where a node's text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRef {
    None,
    /// Not copied: string tables and command names live for the
    /// program.
    Static(&'static str),
    /// A byte range of the tree's text buffer.
    Buf { start: u32, len: u32 },
}

pub mod flag {
    /// A press on it, or Space or Enter while it has the focus, emits
    /// its action.
    pub const ACTS: u16 = 1 << 0;
    /// A stop on the Tab ring.
    pub const FOCUSABLE: u16 = 1 << 1;
    /// Neither hovers, presses nor focuses, and swallows the press.
    pub const DISABLED: u16 = 1 << 2;
    /// Descendants are clipped to its rect, in paint and in hit tests.
    pub const CLIP: u16 = 1 << 3;
    /// Inside a scroll region: drawn and hit only when the whole of it
    /// is in view, never in part.  Text, leaves and rules behave this
    /// way without the flag.
    pub const WHOLE: u16 = 1 << 4;
    /// The text is cut with an ellipsis when it does not fit.
    pub const TRUNCATE: u16 = 1 << 5;
    /// The item of a group that the application's data has selected:
    /// where the focus lands when Tab reaches the group.
    pub const CURRENT: u16 = 1 << 6;
    /// The fill is a plain hard-edged rectangle, painted through
    /// `PaintOut::solid_rect`.
    pub const FLAT: u16 = 1 << 7;
    /// Interface text placed as a control's label is: by the rows
    /// `TextMeasure::centring` gives for it instead of the cap height,
    /// and a whole number of pixels from the top of its box.
    pub const BY_INK: u16 = 1 << 8;
}

/// What positions, sizes and decorates a node.
///
/// Sizes are of the border box: padding is inside them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub padding: Edges,
    pub width: Option<Length>,
    pub height: Option<Length>,
    pub min_w: Option<Length>,
    pub max_w: Option<Length>,
    pub min_h: Option<Length>,
    pub max_h: Option<Length>,
    /// Where the content sits when the box is larger than it.
    pub align: Align,
    /// Share of a parent stack's leftover main-axis space; 0 keeps the
    /// node at its own size.
    pub grow: u16,
    pub fill: Option<Ink>,
    /// Replaces `fill` while the pointer is on the node's widget.
    pub fill_hover: Option<Ink>,
    /// Replaces both while it is held down.
    pub fill_pressed: Option<Ink>,
    pub border: Option<(Length, Ink)>,
    pub radius: Length,
}

impl Style {
    pub const DEFAULT: Style = Style {
        padding: Edges::ZERO,
        width: None,
        height: None,
        min_w: None,
        max_w: None,
        min_h: None,
        max_h: None,
        align: Align::TopLeading,
        grow: 0,
        fill: None,
        fill_hover: None,
        fill_pressed: None,
        border: None,
        radius: Length::ZERO,
    };
}

impl Default for Style {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub kind: Kind,
    pub flags: u16,
    pub(crate) parent: u32,
    pub(crate) first_child: u32,
    pub(crate) last_child: u32,
    pub(crate) next_sibling: u32,
    pub(crate) prev_sibling: u32,
    /// One past the last node of this node's subtree.  Nodes are stored
    /// in the order they are described, so a subtree is the contiguous
    /// range from the node to here.
    pub(crate) subtree_end: u32,
    pub style: Style,

    pub gap: Length,
    pub align_cross: AlignCross,
    pub distribute: Distribute,
    /// A spacer's share of the leftover.
    pub flex: u16,
    pub scroll_slot: u8,

    pub text: TextRef,
    pub font: TextFont,
    pub ink: Ink,

    pub widget: Option<WidgetId>,
    /// Position among the items of the enclosing group, or `NO_ITEM`.
    pub item: u32,
    pub action: Action,
    pub group: GroupKind,

    /// Where a floating node stands, device pixels from the surface's
    /// top-left corner.
    pub float_at: (f64, f64),

    /// Nonzero: the paint output is asked to draw this node itself,
    /// as the preset with this tag.
    pub leaf_tag: u32,
    pub leaf_data: [u32; 2],
}

impl Node {
    pub(crate) fn new(kind: Kind, parent: u32) -> Self {
        Node {
            kind,
            flags: 0,
            parent,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            prev_sibling: NONE,
            subtree_end: 0,
            style: Style::DEFAULT,
            gap: Length::ZERO,
            align_cross: AlignCross::Start,
            distribute: Distribute::Start,
            flex: 1,
            scroll_slot: 0,
            text: TextRef::None,
            font: TextFont::Mono,
            ink: Ink::Role(0),
            widget: None,
            item: NO_ITEM,
            action: Action { id: 0, arg: 0 },
            group: GroupKind::None,
            float_at: (0.0, 0.0),
            leaf_tag: 0,
            leaf_data: [0, 0],
        }
    }

    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }

    /// Whether a scroll region shows this node whole or not at all.
    pub(crate) fn culls_whole(&self) -> bool {
        self.has(flag::WHOLE) || matches!(self.kind, Kind::Text | Kind::Leaf | Kind::Rule)
    }
}
