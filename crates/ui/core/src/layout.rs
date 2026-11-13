//! Layout: a tree and the space it has, to a rectangle per node.
//!
//! The results live in arrays indexed like the node array and are kept
//! between frames, so hit testing and painting read them without
//! laying anything out again.  A child's subtree is a contiguous range
//! of those arrays, which is what makes "the same layout, somewhere
//! else" a loop over a slice instead of a second layout.
//!
//! Everything in here is device pixels.

use crate::geom::{Rect, Size};
use crate::interact::SCROLL_SLOTS;
use crate::metrics::Metrics;
use crate::node::{Kind, NodeId, flag};
use crate::text::{ELLIPSIS, TextFont, TextMeasure};
use crate::tree::Tree;

/// The smallest and largest a node may be on each axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constraints {
    pub min_w: f64,
    pub max_w: f64,
    pub min_h: f64,
    pub max_h: f64,
}

impl Constraints {
    pub fn loose(max_w: f64, max_h: f64) -> Self {
        Self { min_w: 0.0, max_w, min_h: 0.0, max_h }
    }

    pub fn tight(w: f64, h: f64) -> Self {
        Self { min_w: w, max_w: w, min_h: h, max_h: h }
    }

    pub fn clamp(&self, w: f64, h: f64) -> Size {
        Size {
            w: w.max(self.min_w).min(self.max_w).max(0.0),
            h: h.max(self.min_h).min(self.max_h).max(0.0),
        }
    }
}

/// Where a text node's glyphs go, settled at layout so painting does
/// not measure.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextBox {
    /// The box the run is placed by: measured width by cap height for
    /// interface text, cells by row height for monospaced.
    pub w: f64,
    pub h: f64,
    /// Baseline below the top of the box.  For interface text that is
    /// the bottom of the box; descenders hang out of it.
    pub baseline: f64,
    /// Bytes of the node's text that are drawn.
    pub len: u32,
    /// Whether an ellipsis follows them, and where it starts, from the
    /// left of the box.
    pub ellipsis: bool,
    pub ellipsis_at: f64,
}

/// What a scroll region measured: the node, and how tall its content
/// and its viewport came out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollExtent {
    pub node: Option<NodeId>,
    pub content_h: f64,
    pub viewport_h: f64,
}

#[derive(Default)]
pub struct Layout {
    pub(crate) rects: Vec<Rect>,
    pub(crate) inner: Vec<Rect>,
    pub(crate) text: Vec<TextBox>,
    pub(crate) scroll: [ScrollExtent; SCROLL_SLOTS],
    /// The floating nodes, in the order they were described.
    pub(crate) floats: Vec<u32>,
}

impl Layout {
    pub fn new() -> Self {
        Self::default()
    }

    /// The node's border box.
    pub fn rect(&self, id: NodeId) -> Rect {
        self.rects[id.0 as usize]
    }

    /// The box inside the node's padding: what its content fills.
    pub fn inner(&self, id: NodeId) -> Rect {
        self.inner[id.0 as usize]
    }

    pub fn text_box(&self, id: NodeId) -> TextBox {
        self.text[id.0 as usize]
    }

    pub fn scroll_extent(&self, slot: usize) -> ScrollExtent {
        self.scroll[slot]
    }

    /// Lay `tree` out with its root at `origin`.
    pub fn run(
        &mut self,
        tree: &Tree,
        origin: (f64, f64),
        c: Constraints,
        metrics: &Metrics,
        measure: &dyn TextMeasure,
    ) {
        let n = tree.len();
        self.rects.clear();
        self.rects.resize(n, Rect::default());
        self.inner.clear();
        self.inner.resize(n, Rect::default());
        self.text.clear();
        self.text.resize(n, TextBox::default());
        self.scroll = [ScrollExtent::default(); SCROLL_SLOTS];
        self.floats.clear();
        if n == 0 {
            return;
        }
        Pass { tree, m: metrics, measure, out: self }.node(0, origin, c);
        // a floating node was laid out wherever its parent happened to
        // be and moved with it since: put each where it stands, the
        // outer before one inside it
        let mut floats = std::mem::take(&mut self.floats);
        floats.sort_unstable();
        for &f in &floats {
            let at = tree.nodes[f as usize].float_at;
            let r = self.rects[f as usize];
            self.shift(tree, f, origin.0 + at.0 - r.x, origin.1 + at.1 - r.y);
        }
        self.floats = floats;
    }

    pub(crate) fn shift(&mut self, tree: &Tree, i: u32, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let end = tree.nodes[i as usize].subtree_end as usize;
        for r in self.rects[i as usize..end].iter_mut().chain(&mut self.inner[i as usize..end]) {
            r.x += dx;
            r.y += dy;
        }
    }
}

pub(crate) struct Pass<'a> {
    pub(crate) tree: &'a Tree,
    pub(crate) m: &'a Metrics,
    measure: &'a dyn TextMeasure,
    pub(crate) out: &'a mut Layout,
}

impl Pass<'_> {
    /// Lay node `i` out at `origin` and return its border-box size.
    pub(crate) fn node(&mut self, i: u32, origin: (f64, f64), c: Constraints) -> Size {
        let s = self.tree.nodes[i as usize].style;
        let m = self.m;
        let (top, bottom) = (s.padding.top.resolve(c.max_h, m), s.padding.bottom.resolve(c.max_h, m));
        let (left, right) = (s.padding.left.resolve(c.max_w, m), s.padding.right.resolve(c.max_w, m));
        let (pad_w, pad_h) = (left + right, top + bottom);

        let mut ic = Constraints {
            min_w: (c.min_w - pad_w).max(0.0),
            max_w: (c.max_w - pad_w).max(0.0),
            min_h: (c.min_h - pad_h).max(0.0),
            max_h: (c.max_h - pad_h).max(0.0),
        };
        if let Some(w) = s.width {
            ic.min_w = (w.resolve(c.max_w, m) - pad_w).max(0.0);
            ic.max_w = ic.min_w;
        }
        if let Some(h) = s.height {
            ic.min_h = (h.resolve(c.max_h, m) - pad_h).max(0.0);
            ic.max_h = ic.min_h;
        }
        if let Some(v) = s.min_w {
            ic.min_w = ic.min_w.max(v.resolve(c.max_w, m));
        }
        if let Some(v) = s.max_w {
            ic.max_w = ic.max_w.min(v.resolve(c.max_w, m));
        }
        if let Some(v) = s.min_h {
            ic.min_h = ic.min_h.max(v.resolve(c.max_h, m));
        }
        if let Some(v) = s.max_h {
            ic.max_h = ic.max_h.min(v.resolve(c.max_h, m));
        }

        let at = (origin.0 + left, origin.1 + top);
        let content = self.content(i, at, ic);
        let mut size = c.clamp(content.w + pad_w, content.h + pad_h);
        if let Some(w) = s.width {
            size.w = w.resolve(c.max_w, m);
        }
        if let Some(h) = s.height {
            size.h = h.resolve(c.max_h, m);
        }
        self.out.rects[i as usize] = Rect::new(origin.0, origin.1, size.w, size.h);
        self.out.inner[i as usize] = Rect::new(at.0, at.1, content.w, content.h);

        // content smaller than the box it was given: place it
        let (fh, fv) = s.align.factors();
        let (dx, dy) = ((size.w - content.w - pad_w) * fh, (size.h - content.h - pad_h) * fv);
        if dx != 0.0 || dy != 0.0 {
            self.out.shift(self.tree, i, dx, dy);
            let r = &mut self.out.rects[i as usize];
            r.x -= dx;
            r.y -= dy;
        }
        size
    }

    fn content(&mut self, i: u32, at: (f64, f64), c: Constraints) -> Size {
        match self.tree.nodes[i as usize].kind {
            Kind::VStack => self.stack(i, at, c, true),
            Kind::HStack => self.stack(i, at, c, false),
            Kind::ZStack => self.zstack(i, at, c),
            Kind::Scroll => self.scroll(i, at, c),
            Kind::Float => self.stack(i, at, c, true),
            Kind::Text => self.text(i, c),
            Kind::Spacer => c.clamp(c.min_w, c.min_h),
            Kind::Leaf | Kind::Rule => c.clamp(0.0, 0.0),
        }
    }

    fn text(&mut self, i: u32, c: Constraints) -> Size {
        let n = &self.tree.nodes[i as usize];
        let s = self.tree.text_of(n);
        let cut = n.has(flag::TRUNCATE);
        let mut tb = TextBox { len: s.len() as u32, ..TextBox::default() };
        match n.font {
            TextFont::Ui(font) => {
                let fm = self.measure.metrics(font);
                tb.w = self.measure.advance(s, font);
                tb.h = fm.cap_h;
                tb.baseline = fm.cap_h;
                if n.has(flag::BY_INK) {
                    let by = self.measure.centring(s, font);
                    tb.h = by.above + by.below;
                    tb.baseline = by.above;
                }
                if cut && tb.w > c.max_w {
                    let fit = self.measure.fit_prefix(s, font, c.max_w);
                    tb.len = fit.len as u32;
                    tb.w = fit.width;
                    tb.ellipsis = fit.len < s.len();
                    tb.ellipsis_at = fit.width - self.measure.advance(ELLIPSIS, font);
                }
            }
            TextFont::Mono => {
                let cell_w = self.m.cell_w;
                let mut cells = self.measure.mono_cells(s);
                let room = (c.max_w / cell_w).floor().max(0.0);
                if cut && cells as f64 > room {
                    // one cell is kept back for the ellipsis
                    let keep = (room as usize).saturating_sub(1);
                    let (mut used, mut len) = (0usize, 0usize);
                    let mut buf = [0u8; 4];
                    for (at, ch) in s.char_indices() {
                        let w = self.measure.mono_cells(ch.encode_utf8(&mut buf));
                        if used + w > keep {
                            break;
                        }
                        used += w;
                        len = at + ch.len_utf8();
                    }
                    cells = used + 1;
                    tb.len = len as u32;
                    tb.ellipsis = true;
                    tb.ellipsis_at = used as f64 * cell_w;
                }
                tb.w = cells as f64 * cell_w;
                tb.h = self.m.cell_h;
                tb.baseline = self.m.cell_ascent;
            }
        }
        self.out.text[i as usize] = tb;
        c.clamp(tb.w, tb.h)
    }

    /// Lay a floating node out at its own size.  It is put in its
    /// place when everything else has stopped moving.
    pub(crate) fn float(&mut self, i: u32) {
        if !self.out.floats.contains(&i) {
            self.out.floats.push(i);
        }
        self.node(i, (0.0, 0.0), Constraints::loose(f64::INFINITY, f64::INFINITY));
    }

    fn zstack(&mut self, i: u32, origin: (f64, f64), c: Constraints) -> Size {
        let tree = self.tree;
        let (mut w, mut h) = (0.0f64, 0.0f64);
        for ch in tree.children(i) {
            if tree.nodes[ch as usize].kind == Kind::Float {
                self.float(ch);
                continue;
            }
            let s = self.node(ch, origin, c);
            w = w.max(s.w);
            h = h.max(s.h);
        }
        let size = c.clamp(w, h);
        let (fh, fv) = tree.nodes[i as usize].style.align.factors();
        for ch in tree.children(i) {
            if tree.nodes[ch as usize].kind == Kind::Float {
                continue;
            }
            let r = self.out.rects[ch as usize];
            self.out.shift(tree, ch, (size.w - r.w) * fh, (size.h - r.h) * fv);
        }
        size
    }

    /// A column with no ceiling.  The node's own box is the viewport;
    /// the content keeps its unscrolled position, and the offset is
    /// applied when painting and hit testing, so scrolling lays
    /// nothing out.
    fn scroll(&mut self, i: u32, origin: (f64, f64), c: Constraints) -> Size {
        let unbounded = Constraints { min_h: 0.0, max_h: f64::INFINITY, ..c };
        let content = self.stack(i, origin, unbounded, true);
        let viewport = c.clamp(content.w, content.h);
        let slot = self.tree.nodes[i as usize].scroll_slot as usize;
        self.out.scroll[slot] =
            ScrollExtent { node: Some(NodeId(i)), content_h: content.h, viewport_h: viewport.h };
        viewport
    }
}
