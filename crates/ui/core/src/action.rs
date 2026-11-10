//! Identity and what an interaction produces.

/// A widget's identity, given by whoever describes the tree and the
/// same from one rebuild to the next.  A node's index is neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WidgetId(pub u32);

/// What a press or a key on a widget asks for.
///
/// Two numbers and no more: the application has a typed enum on its
/// side and converts.  An event entry point returns one of these by
/// value, so there is no callback to box and no queue to grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Action {
    pub id: u32,
    pub arg: u32,
}

impl Action {
    pub const fn new(id: u32, arg: u32) -> Self {
        Self { id, arg }
    }
}

/// An item's position inside its group; `NO_ITEM` for a widget that is
/// not part of one.
pub const NO_ITEM: u32 = u32::MAX;

/// A widget, or one item of a group widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: WidgetId,
    pub item: u32,
}
