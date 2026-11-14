//! What the pointer and the keyboard have done to a surface.
//!
//! One fixed-size record per surface: which widget is hovered, held
//! and focused, and how far each scroll region is scrolled.  It is
//! keyed by [`WidgetId`], so it outlives every rebuild of the tree it
//! refers to.  Whether a switch is on is not in here — that is the
//! application's data, and it comes back in through the description.
//!
//! Events come in as method calls and go out as an [`Action`] by
//! value.  Every change that alters the picture moves `revision`, and
//! none of them alters a size: state changes colours and the focus
//! ring, so a kept layout stays valid through all of it.

use crate::action::{Action, NO_ITEM, Target};
use crate::hit::hit_test;
use crate::layout::Layout;
use crate::node::{GroupKind, Kind, NONE, Node, flag};
use crate::tree::Tree;

/// Scroll regions a surface can have.
pub const SCROLL_SLOTS: usize = 4;

/// The keys a surface answers.  Everything else is not offered to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    BackTab,
    Space,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyResult {
    /// Not a key this surface has a use for right now.
    Ignored,
    Handled,
    Action(Action),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Interaction {
    pub hovered: Option<Target>,
    pub pressed: Option<Target>,
    pub focused: Option<Target>,
    /// Whether the focus is shown: set by moving it with the keyboard,
    /// cleared by a pointer press.
    pub focus_visible: bool,
    pub scroll: [f64; SCROLL_SLOTS],
    /// How far the surface has been dragged from where it was laid
    /// out.  Added when painting and hit testing; never a relayout.
    pub offset: (f64, f64),
    revision: u64,
}

fn put<T: PartialEq>(slot: &mut T, v: T) -> bool {
    if *slot == v {
        return false;
    }
    *slot = v;
    true
}

fn is_stop(n: &Node) -> bool {
    n.has(flag::FOCUSABLE) && !n.has(flag::DISABLED)
}

impl Interaction {
    /// Moves whenever something that changes the picture did.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self, changed: bool) -> bool {
        self.revision += changed as u64;
        changed
    }

    fn target_at(&self, tree: &Tree, layout: &Layout, p: (f64, f64)) -> Option<(Target, Action)> {
        hit_test(tree, layout, &self.scroll, self.offset, p).map(|h| (h.target, h.action))
    }

    /// True when the hovered widget changed.
    pub fn pointer_move(&mut self, tree: &Tree, layout: &Layout, p: (f64, f64)) -> bool {
        let over = self.target_at(tree, layout, p).map(|(t, _)| t);
        let changed = put(&mut self.hovered, over);
        self.touch(changed)
    }

    pub fn pointer_leave(&mut self) -> bool {
        let changed = put(&mut self.hovered, None);
        self.touch(changed)
    }

    /// A press takes the focus to what it landed on and stops showing
    /// it: the ring is for finding the focus, and the pointer is
    /// already there.
    pub fn pointer_down(&mut self, tree: &Tree, layout: &Layout, p: (f64, f64)) {
        let over = self.target_at(tree, layout, p).map(|(t, _)| t);
        let mut changed = put(&mut self.pressed, over) | put(&mut self.focus_visible, false);
        if let Some(t) = over {
            changed |= put(&mut self.focused, Some(t));
        }
        self.touch(changed);
    }

    /// The action of the widget the press began on, if the release is
    /// on it too.  Pressing, sliding off and letting go does nothing.
    pub fn pointer_up(&mut self, tree: &Tree, layout: &Layout, p: (f64, f64)) -> Option<Action> {
        let held = self.pressed.take();
        self.touch(held.is_some());
        let (over, action) = self.target_at(tree, layout, p)?;
        (held == Some(over)).then_some(action)
    }

    /// Scroll the innermost region under `p` by `dy` device pixels,
    /// positive towards the end of the content.  True when it moved.
    pub fn wheel(&mut self, layout: &Layout, p: (f64, f64), dy: f64) -> bool {
        let q = (p.0 - self.offset.0, p.1 - self.offset.1);
        // a nested region has the larger index
        let slot = (0..SCROLL_SLOTS)
            .filter(|&s| layout.scroll[s].node.is_some_and(|n| layout.rect(n).contains(q)))
            .max_by_key(|&s| layout.scroll[s].node.map(|n| n.0));
        let Some(slot) = slot else { return false };
        let changed = self.scroll_to(layout, slot, self.scroll[slot] + dy);
        self.touch(changed)
    }

    fn scroll_to(&mut self, layout: &Layout, slot: usize, to: f64) -> bool {
        let e = layout.scroll[slot];
        let max = (e.content_h - e.viewport_h).max(0.0);
        put(&mut self.scroll[slot], to.clamp(0.0, max))
    }

    /// Forget the pointer, the focus and every offset, as for a surface
    /// that is shown anew.  The revision moves on rather than starting
    /// again, so a picture painted before is never taken for current.
    pub fn reset(&mut self) {
        *self = Self { revision: self.revision + 1, ..Self::default() };
    }

    /// Put scroll region `slot` at `to` device pixels, for an offset
    /// the application keeps itself.  Held to what the layout allows
    /// the next time one is run.
    pub fn set_scroll(&mut self, slot: usize, to: f64) {
        let changed = put(&mut self.scroll[slot], to.max(0.0));
        self.touch(changed);
    }

    /// Pull every offset back inside what the layout now allows.
    pub fn clamp_scroll(&mut self, layout: &Layout) {
        let mut changed = false;
        for slot in 0..SCROLL_SLOTS {
            changed |= self.scroll_to(layout, slot, self.scroll[slot]);
        }
        self.touch(changed);
    }

    /// Drag the whole surface to `offset` from where it was laid out.
    pub fn set_offset(&mut self, offset: (f64, f64)) {
        let changed = put(&mut self.offset, offset);
        self.touch(changed);
    }

    pub fn key(&mut self, tree: &Tree, layout: &Layout, key: Key) -> KeyResult {
        match key {
            Key::Tab | Key::BackTab => self.tab(tree, layout, key == Key::Tab),
            Key::Space | Key::Enter => {
                let Some(i) = self.focused.and_then(|t| find_acting(tree, t)) else {
                    return KeyResult::Ignored;
                };
                KeyResult::Action(tree.nodes[i as usize].action)
            }
            _ => self.arrow(tree, layout, key),
        }
    }

    /// The Tab ring: stops in the order they were described, round
    /// and round inside this surface.
    fn tab(&mut self, tree: &Tree, layout: &Layout, forward: bool) -> KeyResult {
        let stops = || (0..tree.len() as u32).filter(|&i| is_stop(&tree.nodes[i as usize]));
        let here = self
            .focused
            .and_then(|t| stops().find(|&i| tree.nodes[i as usize].widget == Some(t.id)));
        let next = match (here, forward) {
            (Some(h), true) => stops().find(|&i| i > h).or_else(|| stops().next()),
            (Some(h), false) => stops().rev().find(|&i| i < h).or_else(|| stops().next_back()),
            (None, true) => stops().next(),
            (None, false) => stops().next_back(),
        };
        let Some(next) = next else { return KeyResult::Ignored };
        let n = &tree.nodes[next as usize];
        let item = match n.group {
            GroupKind::None => NO_ITEM,
            _ => landing_item(tree, next),
        };
        let Some(id) = n.widget else { return KeyResult::Ignored };
        self.focus(tree, layout, Target { id, item });
        KeyResult::Handled
    }

    fn arrow(&mut self, tree: &Tree, layout: &Layout, key: Key) -> KeyResult {
        let Some(at) = self.focused else { return KeyResult::Ignored };
        let Some(group) = (0..tree.len() as u32).find(|&i| {
            let n = &tree.nodes[i as usize];
            n.widget == Some(at.id) && n.group != GroupKind::None
        }) else {
            return KeyResult::Ignored;
        };
        let kind = tree.nodes[group as usize].group;
        let items = || items_of(tree, group);
        let before = || items().filter(|&(_, it)| it < at.item).last();
        let after = || items().find(|&(_, it)| it > at.item);
        let to = match (kind, key) {
            (GroupKind::Segmented, Key::Left) | (GroupKind::List, Key::Up) => before(),
            (GroupKind::Segmented, Key::Right) | (GroupKind::List, Key::Down) => after(),
            (GroupKind::List, Key::Home) => items().next(),
            (GroupKind::List, Key::End) => items().last(),
            _ => return KeyResult::Ignored,
        };
        // at the end of the group the key is spent and the focus stays
        let Some((node, item)) = to else {
            let shown = put(&mut self.focus_visible, true);
            self.touch(shown);
            return KeyResult::Handled;
        };
        let moved = item != at.item;
        self.focus(tree, layout, Target { id: at.id, item });
        if kind == GroupKind::Segmented && moved {
            KeyResult::Action(tree.nodes[node as usize].action)
        } else {
            KeyResult::Handled
        }
    }

    /// Move the keyboard focus, show it, and bring it into view.
    fn focus(&mut self, tree: &Tree, layout: &Layout, t: Target) {
        let mut changed = put(&mut self.focused, Some(t)) | put(&mut self.focus_visible, true);
        if let Some(i) = find_focus_node(tree, t)
            && let Some(region) = tree.ancestor_of_kind(i, Kind::Scroll)
        {
            let slot = tree.nodes[region as usize].scroll_slot as usize;
            let view = layout.inner[region as usize];
            let r = layout.rects[i as usize];
            let at = self.scroll[slot];
            let to = if r.y - at < view.y {
                r.y - view.y
            } else if r.bottom() - at > view.bottom() {
                r.bottom() - view.bottom()
            } else {
                at
            };
            changed |= self.scroll_to(layout, slot, to);
        }
        self.touch(changed);
    }
}

/// The enabled items of the group at `group`, as `(node, item)`.
fn items_of(tree: &Tree, group: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
    let g = &tree.nodes[group as usize];
    (group + 1..g.subtree_end).filter_map(move |i| {
        let n = &tree.nodes[i as usize];
        (n.widget == g.widget && n.item != NO_ITEM && !n.has(flag::DISABLED)).then_some((i, n.item))
    })
}

/// Where the focus lands in a group reached by Tab: the item the
/// application has selected, or the first.
fn landing_item(tree: &Tree, group: u32) -> u32 {
    items_of(tree, group)
        .find(|&(i, _)| tree.nodes[i as usize].has(flag::CURRENT))
        .or_else(|| items_of(tree, group).next())
        .map_or(NO_ITEM, |(_, item)| item)
}

/// The node Space or Enter acts on for a focused target.
fn find_acting(tree: &Tree, t: Target) -> Option<u32> {
    (0..tree.len() as u32).find(|&i| {
        let n = &tree.nodes[i as usize];
        n.widget == Some(t.id) && n.item == t.item && n.has(flag::ACTS) && !n.has(flag::DISABLED)
    })
}

/// The node the focus ring goes round: the item when the target names
/// one, the widget otherwise.
pub(crate) fn find_focus_node(tree: &Tree, t: Target) -> Option<u32> {
    let mut widget = NONE;
    for (i, n) in tree.nodes.iter().enumerate() {
        if n.widget != Some(t.id) {
            continue;
        }
        if n.item == t.item {
            return Some(i as u32);
        }
        if widget == NONE {
            widget = i as u32;
        }
    }
    (widget != NONE).then_some(widget)
}
