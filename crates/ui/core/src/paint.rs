//! Painting: a laid-out tree to calls on an output.
//!
//! The tree does not know what a glyph or an instance buffer is.  It
//! walks itself once and tells a [`PaintOut`] where each rectangle and
//! run of text goes; what the output keeps from that is the surface's
//! retained picture, and a frame whose inputs did not change is that
//! picture again, with no walk.
//!
//! Interaction state is read here and only here: a hover changes which
//! fill a node asks for and nothing about where it is.

use crate::action::{NO_ITEM, Target};
use crate::geom::Rect;
use crate::hit::in_view;
use crate::interact::{Interaction, find_focus_node};
use crate::layout::Layout;
use crate::metrics::Metrics;
use crate::node::{Ink, Kind, NONE, Node, flag};
use crate::text::{ELLIPSIS, TextFont};
use crate::tree::Tree;

/// What the pointer and keyboard are doing to the widget a node
/// belongs to.  The `_item` fields say which item of a group, for a
/// preset that draws its items itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetState {
    pub hovered: bool,
    pub pressed: bool,
    /// Focused, and the focus is being shown.
    pub focused: bool,
    pub disabled: bool,
    pub hovered_item: u32,
    pub pressed_item: u32,
    pub focused_item: u32,
}

impl WidgetState {
    pub const IDLE: WidgetState = WidgetState {
        hovered: false,
        pressed: false,
        focused: false,
        disabled: false,
        hovered_item: NO_ITEM,
        pressed_item: NO_ITEM,
        focused_item: NO_ITEM,
    };
}

/// Where a surface's picture goes.  Coordinates are device pixels.
pub trait PaintOut {
    /// Forget the picture; a whole new one follows.
    fn reset(&mut self);
    /// A plain rectangle with hard edges, for a node marked flat.  An
    /// output that draws these in a pass of their own draws them under
    /// everything `rounded_rect` paints.
    fn solid_rect(&mut self, rect: Rect, ink: Ink);
    /// What every other fill and border is, square corners included.
    fn rounded_rect(&mut self, rect: Rect, radius: f64, fill: Option<Ink>, border: Option<(f64, Ink)>);
    /// A run of text starting at `x` with its baseline at `baseline`.
    fn text_run(&mut self, x: f64, baseline: f64, text: &str, font: TextFont, ink: Ink);
    /// A preset component, drawn by whoever knows what `tag` means.
    fn leaf(&mut self, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState);
    /// The keyboard focus, shown round `rect`.
    fn focus_ring(&mut self, rect: Rect, radius: f64);
    fn push_clip(&mut self, rect: Rect);
    fn pop_clip(&mut self);
}

struct Painter<'a, O: PaintOut> {
    tree: &'a Tree,
    layout: &'a Layout,
    state: &'a Interaction,
    m: &'a Metrics,
    ring: u32,
    ring_at: Option<(Rect, f64)>,
    out: &'a mut O,
}

fn on_widget(t: Option<Target>, n: &Node) -> (bool, u32) {
    match (t, n.widget) {
        (Some(t), Some(id)) if t.id == id && (n.item == NO_ITEM || n.item == t.item) => (true, t.item),
        _ => (false, NO_ITEM),
    }
}

impl<O: PaintOut> Painter<'_, O> {
    fn widget_state(&self, n: &Node) -> WidgetState {
        let disabled = n.has(flag::DISABLED);
        let (hovered, hovered_item) = on_widget(self.state.hovered, n);
        let (pressed, pressed_item) = on_widget(self.state.pressed, n);
        let (focused, focused_item) = on_widget(self.state.focused, n);
        WidgetState {
            hovered: hovered && !disabled,
            pressed: pressed && !disabled,
            focused: focused && self.state.focus_visible,
            disabled,
            hovered_item,
            pressed_item,
            focused_item,
        }
    }

    fn node(&mut self, i: u32, dx: f64, dy: f64, view: Option<Rect>) {
        let n = &self.tree.nodes[i as usize];
        let r = self.layout.rects[i as usize].translated(dx, dy);
        if !in_view(n, &r, view.as_ref()) {
            return;
        }
        let ws = self.widget_state(n);
        let s = &n.style;
        let radius = s.radius.resolve(0.0, self.m);

        let fill = match (n.has(flag::ACTS), ws.pressed, ws.hovered) {
            (true, true, _) => s.fill_pressed.or(s.fill_hover).or(s.fill),
            (true, false, true) => s.fill_hover.or(s.fill),
            _ => s.fill,
        };
        // a container half out of a scroll region keeps the part that is in
        let own = view.filter(|_| !n.culls_whole()).map_or(r, |v| v.intersect(&r));
        let border = s.border.map(|(w, ink)| (w.resolve(0.0, self.m), ink));
        match (fill, border) {
            (None, None) => {}
            (Some(ink), None) if n.has(flag::FLAT) => self.out.solid_rect(own, ink),
            _ => self.out.rounded_rect(own, radius, fill, border),
        }
        if n.leaf_tag != 0 {
            self.out.leaf(n.leaf_tag, n.leaf_data, self.tree.text_of(n), r, ws);
        }
        if n.kind == Kind::Text {
            let tb = self.layout.text[i as usize];
            let room = self.layout.inner[i as usize].translated(dx, dy);
            let mut at = room.place(tb.w, tb.h, s.align);
            if n.has(flag::BY_INK) {
                // the rows of ink are whole, and so is where they start
                at.y = room.y + (at.y - room.y + 0.5).floor();
            }
            let text = &self.tree.text_of(n)[..tb.len as usize];
            self.out.text_run(at.x, at.y + tb.baseline, text, n.font, n.ink);
            if tb.ellipsis {
                self.out.text_run(at.x + tb.ellipsis_at, at.y + tb.baseline, ELLIPSIS, n.font, n.ink);
            }
        }

        let clip = n.has(flag::CLIP);
        if clip {
            self.out.push_clip(r);
        }
        let (child_dy, child_view) = if n.kind == Kind::Scroll {
            let viewport = self.layout.inner[i as usize].translated(dx, dy);
            (dy - self.state.scroll[n.scroll_slot as usize], Some(viewport))
        } else {
            (dy, view)
        };
        let mut ch = n.first_child;
        while ch != NONE {
            self.node(ch, dx, child_dy, child_view);
            ch = self.tree.nodes[ch as usize].next_sibling;
        }
        if clip {
            self.out.pop_clip();
        }
        if i == self.ring {
            self.ring_at = Some((r, radius));
        }
    }
}

/// Walk the tree once and paint it into `out`.
pub fn paint<O: PaintOut>(
    tree: &Tree,
    layout: &Layout,
    state: &Interaction,
    metrics: &Metrics,
    out: &mut O,
) {
    out.reset();
    if tree.is_empty() {
        return;
    }
    let ring = state
        .focused
        .filter(|_| state.focus_visible)
        .and_then(|t| find_focus_node(tree, t))
        .unwrap_or(NONE);
    let (dx, dy) = state.offset;
    let mut p = Painter { tree, layout, state, m: metrics, ring, ring_at: None, out };
    p.node(0, dx, dy, None);
    // last, and outside every clip: the ring is drawn round the widget,
    // where the widget's own container would cut it
    if let Some((rect, radius)) = p.ring_at {
        p.out.focus_ring(rect, radius);
    }
}

/// One call on a [`PaintOut`], kept.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Solid { rect: Rect, ink: Ink },
    Rounded { rect: Rect, radius: f64, fill: Option<Ink>, border: Option<(f64, Ink)> },
    /// The text is `start..start + len` of the recording's buffer.
    Text { x: f64, baseline: f64, start: u32, len: u32, font: TextFont, ink: Ink },
    Leaf { tag: u32, data: [u32; 2], start: u32, len: u32, rect: Rect, state: WidgetState },
    FocusRing { rect: Rect, radius: f64 },
    PushClip(Rect),
    PopClip,
}

/// An output that keeps the calls it was given, for tests and for
/// anything that wants to look at a picture as data.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recording {
    pub ops: Vec<Op>,
    text: String,
}

impl Recording {
    pub fn new() -> Self {
        Self::default()
    }

    /// The text of a `Text` or `Leaf` op.
    pub fn text(&self, op: &Op) -> &str {
        match *op {
            Op::Text { start, len, .. } | Op::Leaf { start, len, .. } => {
                &self.text[start as usize..(start + len) as usize]
            }
            _ => "",
        }
    }

    fn keep(&mut self, s: &str) -> (u32, u32) {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        (start, s.len() as u32)
    }
}

impl PaintOut for Recording {
    fn reset(&mut self) {
        self.ops.clear();
        self.text.clear();
    }

    fn solid_rect(&mut self, rect: Rect, ink: Ink) {
        self.ops.push(Op::Solid { rect, ink });
    }

    fn rounded_rect(&mut self, rect: Rect, radius: f64, fill: Option<Ink>, border: Option<(f64, Ink)>) {
        self.ops.push(Op::Rounded { rect, radius, fill, border });
    }

    fn text_run(&mut self, x: f64, baseline: f64, text: &str, font: TextFont, ink: Ink) {
        let (start, len) = self.keep(text);
        self.ops.push(Op::Text { x, baseline, start, len, font, ink });
    }

    fn leaf(&mut self, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState) {
        let (start, len) = self.keep(label);
        self.ops.push(Op::Leaf { tag, data, start, len, rect, state });
    }

    fn focus_ring(&mut self, rect: Rect, radius: f64) {
        self.ops.push(Op::FocusRing { rect, radius });
    }

    fn push_clip(&mut self, rect: Rect) {
        self.ops.push(Op::PushClip(rect));
    }

    fn pop_clip(&mut self) {
        self.ops.push(Op::PopClip);
    }
}
