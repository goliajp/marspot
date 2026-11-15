//! Which widget is under a point.
//!
//! One walk over the kept layout, children before parents and later
//! siblings before earlier ones — the reverse of the order they are
//! painted in, so what is drawn on top is what is found first.  Nothing
//! is measured and nothing is laid out: a pointer moving over a surface
//! costs a tree walk and no more.

use crate::action::{Action, SPOT, Target};
use crate::geom::Rect;
use crate::layout::Layout;
use crate::node::{Kind, NONE, Node, NodeId, flag};
use crate::tree::Tree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub node: NodeId,
    pub target: Target,
    pub action: Action,
}

/// Whether a scroll region with viewport `view` shows the node at `r`.
///
/// Text, leaves, rules and anything marked whole are shown only when
/// all of them is in view: half a row and half a glyph are what a clip
/// would have prevented.  Other containers are shown while any of them
/// is, and their own fill is cut to the viewport.
pub(crate) fn in_view(node: &Node, r: &Rect, view: Option<&Rect>) -> bool {
    match view {
        None => true,
        Some(v) if node.culls_whole() => v.holds(r),
        Some(v) => !v.intersect(r).is_empty(),
    }
}

enum Found {
    Nothing,
    /// A disabled widget is there: nothing reacts, and nothing under
    /// it does either.
    Blocked,
    /// The node, and its spot's action when the point is on its spot.
    Node(u32, Option<Action>),
}

struct Walk<'a> {
    tree: &'a Tree,
    layout: &'a Layout,
    scroll: &'a [f64],
    p: (f64, f64),
}

impl Walk<'_> {
    /// A node is looked at only when the point is inside its box, so
    /// by the time one is reached the point is inside every ancestor's
    /// box too.  That is the clip: a child that overflows a clipping
    /// parent, or content scrolled out of a viewport, is never hit
    /// where it is not shown.
    fn node(&self, i: u32, dy: f64, view: Option<Rect>) -> Found {
        let n = &self.tree.nodes[i as usize];
        let r = self.layout.rects[i as usize].translated(0.0, dy);
        if !in_view(n, &r, view.as_ref()) || !r.contains(self.p) {
            return Found::Nothing;
        }
        if n.has(flag::DISABLED) {
            return Found::Blocked;
        }
        let (mut dy, mut view) = (dy, view);
        if n.kind == Kind::Scroll {
            let viewport = self.layout.inner[i as usize].translated(0.0, dy);
            if !viewport.contains(self.p) {
                return Found::Nothing;
            }
            view = Some(viewport);
            dy -= self.scroll[n.scroll_slot as usize];
        }
        if n.has(flag::ROWS) && n.first_child != NONE {
            return self.row(i, n, dy, view);
        }
        let mut ch = n.last_child;
        while ch != NONE {
            // a floating node is not where it was described
            if self.tree.nodes[ch as usize].kind != Kind::Float {
                match self.node(ch, dy, view) {
                    Found::Nothing => {}
                    found => return found,
                }
            }
            ch = self.tree.nodes[ch as usize].prev_sibling;
        }
        self.acting(i, n, &r)
    }

    fn acting(&self, i: u32, n: &Node, r: &Rect) -> Found {
        if !n.has(flag::ACTS) {
            return Found::Nothing;
        }
        let spot = self.tree.spot_of(n).filter(|s| s.rect.translated(r.x, r.y).contains(self.p));
        Found::Node(i, spot.map(|s| s.action))
    }

    /// The point in a column of rows of one height: only the row it is
    /// at the height of is looked at.
    fn row(&self, i: u32, n: &Node, dy: f64, view: Option<Rect>) -> Found {
        let first = self.layout.rects[n.first_child as usize].translated(0.0, dy);
        let stride = self.tree.nodes[n.first_child as usize].subtree_end - n.first_child;
        let at = ((self.p.1 - first.y) / first.h).floor();
        // past the last row is past the column's subtree
        let ch = if first.h > 0.0 && at >= 0.0 && at < u32::MAX as f64 {
            (at as u64 * stride as u64 + n.first_child as u64).min(n.subtree_end as u64) as u32
        } else {
            n.subtree_end
        };
        if ch < n.subtree_end {
            debug_assert_eq!(self.tree.nodes[ch as usize].parent, i, "the rows of a column are one size");
            let c = &self.tree.nodes[ch as usize];
            let found = if c.first_child == NONE && c.kind != Kind::Scroll {
                // a row with nothing in it: the walk below, without the
                // parts of it a leaf has no use for
                let r = self.layout.rects[ch as usize].translated(0.0, dy);
                if !in_view(c, &r, view.as_ref()) || !r.contains(self.p) {
                    Found::Nothing
                } else if c.has(flag::DISABLED) {
                    Found::Blocked
                } else {
                    self.acting(ch, c, &r)
                }
            } else {
                self.node(ch, dy, view)
            };
            if !matches!(found, Found::Nothing) {
                return found;
            }
        }
        let r = self.layout.rects[i as usize].translated(0.0, dy);
        self.acting(i, n, &r)
    }
}

/// The widget at `p`.  `scroll` is the surface's scroll offsets and
/// `offset` how far the whole surface has been dragged from where it
/// was laid out.
pub fn hit_test(
    tree: &Tree,
    layout: &Layout,
    scroll: &[f64],
    offset: (f64, f64),
    p: (f64, f64),
) -> Option<Hit> {
    if tree.is_empty() {
        return None;
    }
    let walk = Walk { tree, layout, scroll, p: (p.0 - offset.0, p.1 - offset.1) };
    // what floats is on top: the last described first, then the rest
    let mut found = Found::Nothing;
    // a pointer over none of them, as it mostly is, walks none of them
    let floats = if layout.float_bounds.contains(walk.p) { &layout.floats[..] } else { &[] };
    for &f in floats.iter().rev() {
        found = walk.node(f, 0.0, None);
        if !matches!(found, Found::Nothing) {
            break;
        }
    }
    if matches!(found, Found::Nothing) {
        found = walk.node(0, 0.0, None);
    }
    let Found::Node(i, spot) = found else { return None };
    let n = &tree.nodes[i as usize];
    let id = n.widget?;
    let (part, action) = match spot {
        Some(a) => (SPOT, a),
        None => (0, n.action),
    };
    Some(Hit { node: NodeId(i), target: Target { id, item: n.item, part }, action })
}
