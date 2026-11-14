//! The arena a tree is described into, and the handle that describes it.
//!
//! A surface is described from scratch whenever its data changes:
//! `ui.vstack(|ui| { ui.text(..); ui.leaf(..); })`.  The description
//! lands in two buffers that are cleared, not freed, so the second
//! description of a tree the same size takes nothing from the heap.
//! The caller keeps no tree and marks nothing dirty.

use core::fmt::{self, Write};

use crate::action::{Action, WidgetId};
use crate::geom::Align;
use crate::metrics::{Edges, Length};
use crate::node::{AlignCross, Distribute, GroupKind, Ink, Kind, NONE, Node, NodeId, TextRef, flag};
use crate::text::TextFont;

#[derive(Default)]
pub struct Tree {
    pub(crate) nodes: Vec<Node>,
    pub(crate) text: String,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Describe the tree again.  What was there is dropped; the
    /// buffers keep their capacity.
    pub fn build(&mut self, f: impl FnOnce(&mut Ui<'_>)) {
        self.nodes.clear();
        self.text.clear();
        let mut ui = Ui { tree: self, parent: NONE, group: None, next_item: 0 };
        f(&mut ui);
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    pub fn text_of(&self, n: &Node) -> &str {
        match n.text {
            TextRef::None => "",
            TextRef::Static(s) => s,
            TextRef::Buf { start, len } => &self.text[start as usize..(start + len) as usize],
        }
    }

    /// Bytes of text the description copied into the buffer.
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.nodes[id.0 as usize].parent;
        (p != NONE).then_some(NodeId(p))
    }

    /// The node's children, in the order they were described.
    pub fn children_of(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id.0).map(NodeId)
    }

    pub(crate) fn children(&self, i: u32) -> Children<'_> {
        Children { nodes: &self.nodes, next: self.nodes[i as usize].first_child }
    }

    /// The node `i`'s nearest ancestor of `kind`.
    pub(crate) fn ancestor_of_kind(&self, i: u32, kind: Kind) -> Option<u32> {
        let mut p = self.nodes[i as usize].parent;
        while p != NONE {
            if self.nodes[p as usize].kind == kind {
                return Some(p);
            }
            p = self.nodes[p as usize].parent;
        }
        None
    }
}

pub(crate) struct Children<'a> {
    nodes: &'a [Node],
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let i = self.next;
        self.next = self.nodes[i as usize].next_sibling;
        Some(i)
    }
}

/// Describes a tree, one node at a time, into its arena.
pub struct Ui<'t> {
    tree: &'t mut Tree,
    parent: u32,
    group: Option<WidgetId>,
    next_item: u32,
}

impl<'t> Ui<'t> {
    fn push(&mut self, kind: Kind) -> u32 {
        let i = self.tree.nodes.len() as u32;
        debug_assert!(self.parent != NONE || i == 0, "a tree has one root");
        let mut node = Node::new(kind, self.parent);
        node.subtree_end = i + 1;
        if self.parent != NONE {
            let p = &mut self.tree.nodes[self.parent as usize];
            node.prev_sibling = p.last_child;
            let prev = core::mem::replace(&mut p.last_child, i);
            if prev == NONE {
                p.first_child = i;
            } else {
                self.tree.nodes[prev as usize].next_sibling = i;
            }
        }
        self.tree.nodes.push(node);
        i
    }

    fn container(&mut self, kind: Kind, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        let i = self.push(kind);
        let outer = core::mem::replace(&mut self.parent, i);
        f(self);
        self.parent = outer;
        self.tree.nodes[i as usize].subtree_end = self.tree.nodes.len() as u32;
        At { ui: self, n: i }
    }

    pub fn vstack(&mut self, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        self.container(Kind::VStack, f)
    }

    pub fn hstack(&mut self, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        self.container(Kind::HStack, f)
    }

    /// Children on top of one another, each placed by `align`.
    pub fn zstack(&mut self, align: Align, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        self.container(Kind::ZStack, f).align(align)
    }

    /// A column that scrolls.  `slot` names which of the surface's
    /// scroll offsets it follows.
    pub fn scroll(&mut self, slot: u8, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        debug_assert!((slot as usize) < crate::interact::SCROLL_SLOTS);
        self.container(Kind::Scroll, f).set(|n| n.scroll_slot = slot)
    }

    /// A column floating over the surface with its corner `(x, y)`
    /// device pixels from the surface's: see `Kind::Float`.
    pub fn float(&mut self, x: f64, y: f64, f: impl FnOnce(&mut Ui<'t>)) -> At<'_, 't> {
        self.container(Kind::Float, f).set(|n| n.float_at = (x, y))
    }

    /// A widget made of items: one stop on the Tab ring, the arrow
    /// keys move inside it.  Every node marked `.item(..)` in `f`
    /// becomes one of its items, numbered in order.
    pub fn group(
        &mut self,
        id: WidgetId,
        kind: GroupKind,
        vertical: bool,
        f: impl FnOnce(&mut Ui<'t>),
    ) -> At<'_, 't> {
        let outer = (self.group.replace(id), core::mem::replace(&mut self.next_item, 0));
        let stack = if vertical { Kind::VStack } else { Kind::HStack };
        let i = self.container(stack, f).n;
        (self.group, self.next_item) = outer;
        let n = &mut self.tree.nodes[i as usize];
        n.widget = Some(id);
        n.group = kind;
        n.flags |= flag::FOCUSABLE;
        At { ui: self, n: i }
    }

    fn text_node(&mut self, font: TextFont, ink: Ink, text: TextRef) -> At<'_, 't> {
        let i = self.push(Kind::Text);
        let n = &mut self.tree.nodes[i as usize];
        n.font = font;
        n.ink = ink;
        n.text = text;
        At { ui: self, n: i }
    }

    /// Text that lives for the program; nothing is copied.
    pub fn text(&mut self, font: TextFont, ink: Ink, s: &'static str) -> At<'_, 't> {
        self.text_node(font, ink, TextRef::Static(s))
    }

    /// Text made at run time, copied into the tree's buffer.
    pub fn text_copy(&mut self, font: TextFont, ink: Ink, s: &str) -> At<'_, 't> {
        let start = self.tree.text.len() as u32;
        self.tree.text.push_str(s);
        self.text_node(font, ink, TextRef::Buf { start, len: s.len() as u32 })
    }

    /// Text formatted straight into the tree's buffer:
    /// `ui.text_fmt(font, ink, format_args!("{pct}%"))`.
    pub fn text_fmt(&mut self, font: TextFont, ink: Ink, args: fmt::Arguments<'_>) -> At<'_, 't> {
        let start = self.tree.text.len();
        // writing to a String cannot fail
        let _ = self.tree.text.write_fmt(args);
        let len = (self.tree.text.len() - start) as u32;
        self.text_node(font, ink, TextRef::Buf { start: start as u32, len })
    }

    /// Text written by `write` straight into the tree's buffer, for
    /// a sentence a string table composes around a value.  `write`
    /// only appends.
    pub fn text_with(&mut self, font: TextFont, ink: Ink, write: impl FnOnce(&mut String)) -> At<'_, 't> {
        let start = self.tree.text.len();
        write(&mut self.tree.text);
        let len = (self.tree.text.len() - start) as u32;
        self.text_node(font, ink, TextRef::Buf { start: start as u32, len })
    }

    /// Takes the space a stack has left over.
    pub fn spacer(&mut self) -> At<'_, 't> {
        let i = self.push(Kind::Spacer);
        At { ui: self, n: i }
    }

    /// A box of a given size with nothing in it.  With a nonzero `tag`
    /// the paint output draws it as that preset.
    pub fn leaf(&mut self, tag: u32, w: Length, h: Length) -> At<'_, 't> {
        let i = self.push(Kind::Leaf);
        let n = &mut self.tree.nodes[i as usize];
        n.leaf_tag = tag;
        n.style.width = Some(w);
        n.style.height = Some(h);
        At { ui: self, n: i }
    }

    /// A separator across the enclosing stack, drawn at the edge
    /// between the children on either side and taking no room.
    pub fn rule(&mut self, thickness: Length, ink: Ink) -> At<'_, 't> {
        let i = self.push(Kind::Rule);
        let n = &mut self.tree.nodes[i as usize];
        n.style.height = Some(thickness);
        n.style.fill = Some(ink);
        At { ui: self, n: i }
    }
}

/// The node just described, for setting what it does not have by
/// default.
pub struct At<'u, 't> {
    ui: &'u mut Ui<'t>,
    n: u32,
}

impl At<'_, '_> {
    fn node(&mut self) -> &mut Node {
        &mut self.ui.tree.nodes[self.n as usize]
    }

    fn set(mut self, f: impl FnOnce(&mut Node)) -> Self {
        f(self.node());
        self
    }

    fn flag(self, f: u16, on: bool) -> Self {
        self.set(|n| if on { n.flags |= f } else { n.flags &= !f })
    }

    pub fn id(&self) -> NodeId {
        NodeId(self.n)
    }

    /// Place the text as the label of a control: see `flag::BY_INK`.
    pub fn by_ink(self) -> Self {
        self.flag(flag::BY_INK, true)
    }

    pub fn padding(self, e: Edges) -> Self {
        self.set(|n| n.style.padding = e)
    }

    pub fn width(self, l: Length) -> Self {
        self.set(|n| n.style.width = Some(l))
    }

    pub fn height(self, l: Length) -> Self {
        self.set(|n| n.style.height = Some(l))
    }

    pub fn min_w(self, l: Length) -> Self {
        self.set(|n| n.style.min_w = Some(l))
    }

    pub fn max_w(self, l: Length) -> Self {
        self.set(|n| n.style.max_w = Some(l))
    }

    pub fn min_h(self, l: Length) -> Self {
        self.set(|n| n.style.min_h = Some(l))
    }

    pub fn max_h(self, l: Length) -> Self {
        self.set(|n| n.style.max_h = Some(l))
    }

    /// Where the content sits when the box is larger than it.
    pub fn align(self, a: Align) -> Self {
        self.set(|n| n.style.align = a)
    }

    /// Take a share of the parent stack's leftover main-axis space.
    pub fn grow(self, share: u16) -> Self {
        self.set(|n| n.style.grow = share)
    }

    pub fn gap(self, l: Length) -> Self {
        self.set(|n| n.gap = l)
    }

    pub fn align_cross(self, a: AlignCross) -> Self {
        self.set(|n| n.align_cross = a)
    }

    pub fn distribute(self, d: Distribute) -> Self {
        self.set(|n| n.distribute = d)
    }

    /// A spacer's share of the leftover, when spacers are unequal.
    pub fn flex(self, share: u16) -> Self {
        self.set(|n| n.flex = share)
    }

    pub fn fill(self, ink: Ink) -> Self {
        self.set(|n| n.style.fill = Some(ink))
    }

    pub fn fill_hover(self, ink: Ink) -> Self {
        self.set(|n| n.style.fill_hover = Some(ink))
    }

    pub fn fill_pressed(self, ink: Ink) -> Self {
        self.set(|n| n.style.fill_pressed = Some(ink))
    }

    pub fn border(self, width: Length, ink: Ink) -> Self {
        self.set(|n| n.style.border = Some((width, ink)))
    }

    pub fn radius(self, l: Length) -> Self {
        self.set(|n| n.style.radius = l)
    }

    /// Fill with a plain hard-edged rectangle, under everything else.
    pub fn flat(self) -> Self {
        self.flag(flag::FLAT, true)
    }

    pub fn clip(self) -> Self {
        self.flag(flag::CLIP, true)
    }

    /// Inside a scroll region, shown whole or not at all.
    pub fn whole(self) -> Self {
        self.flag(flag::WHOLE, true)
    }

    /// Cut the text with an ellipsis when it does not fit.
    pub fn truncate(self) -> Self {
        self.flag(flag::TRUNCATE, true)
    }

    pub fn disabled(self, on: bool) -> Self {
        self.flag(flag::DISABLED, on)
    }

    /// A widget of its own: focusable, and a press or Space or Enter
    /// emits `action`.
    pub fn widget(self, id: WidgetId, action: Action) -> Self {
        self.set(|n| {
            n.widget = Some(id);
            n.action = action;
            n.flags |= flag::ACTS | flag::FOCUSABLE;
        })
    }

    /// Answers a press with `action` and is no stop for Tab: a drag
    /// handle, a row the keyboard reaches some other way.
    pub fn pressable(self, id: WidgetId, action: Action) -> Self {
        self.set(|n| {
            n.widget = Some(id);
            n.action = action;
            n.flags |= flag::ACTS;
        })
    }

    /// The next item of the enclosing group.
    pub fn item(self, action: Action) -> Self {
        let group = self.ui.group;
        debug_assert!(group.is_some(), "an item outside a group");
        let index = self.ui.next_item;
        self.ui.next_item += 1;
        self.set(|n| {
            n.widget = group;
            n.item = index;
            n.action = action;
            n.flags |= flag::ACTS;
        })
    }

    /// Light this node, with its hover fill, whenever the pointer is
    /// on a node of `group`; see `Node::lit_group`.
    pub fn lit_with(self, group: u32) -> Self {
        self.set(|n| n.lit_group = group)
    }

    /// The item the application's data has selected.
    pub fn current(self, on: bool) -> Self {
        self.flag(flag::CURRENT, on)
    }

    /// Have the paint output draw this node as the preset `tag`.
    pub fn leaf(self, tag: u32, data: [u32; 2]) -> Self {
        self.set(|n| {
            n.leaf_tag = tag;
            n.leaf_data = data;
        })
    }

    /// The text a preset draws itself, handed to the paint output
    /// with the node.
    pub fn label(self, s: &'static str) -> Self {
        self.set(|n| n.text = TextRef::Static(s))
    }

    /// [`Self::label`] for text made at run time, copied into the
    /// tree's buffer.
    pub fn label_copy(self, s: &str) -> Self {
        let start = self.ui.tree.text.len() as u32;
        self.ui.tree.text.push_str(s);
        self.set(|n| n.text = TextRef::Buf { start, len: s.len() as u32 })
    }

    /// [`Self::label_copy`] for text `write` appends to the tree's
    /// buffer itself: a label made of several parts.
    pub fn label_with(self, write: impl FnOnce(&mut String)) -> Self {
        let start = self.ui.tree.text.len();
        write(&mut self.ui.tree.text);
        let len = (self.ui.tree.text.len() - start) as u32;
        self.set(|n| n.text = TextRef::Buf { start: start as u32, len })
    }
}
