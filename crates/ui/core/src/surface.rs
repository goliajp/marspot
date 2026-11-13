//! A surface: one independently rebuilt piece of interface.
//!
//! A panel, a menu, a toolbar.  It keeps its tree, its layout, its
//! interaction state and its painted output from frame to frame, and
//! each frame does only as much as its inputs say has gone stale:
//!
//! | what changed | what runs |
//! |---|---|
//! | the data or the theme | describe, lay out, paint |
//! | the space, the scale, the fonts | lay out, paint |
//! | hover, press, focus, scroll, drag, the glyph atlas | paint |
//! | nothing | nothing: the kept output is the frame |
//!
//! The three counters say how many times each stage has run, so a test
//! can hold an idle frame to having run none of them.

use crate::action::Action;
use crate::hit::{Hit, hit_test};
use crate::interact::{Interaction, Key, KeyResult};
use crate::layout::{Constraints, Layout};
use crate::metrics::Metrics;
use crate::paint::{PaintOut, paint};
use crate::text::TextMeasure;
use crate::tree::{Tree, Ui};

/// Everything outside a surface that its picture depends on.  A
/// surface compares these with the ones it last saw; it never looks at
/// another surface's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Inputs {
    /// Moved by whoever owns the data, at every place it is changed.
    pub data_rev: u64,
    pub theme_rev: u64,
    /// Top-left of the surface, device pixels.
    pub origin: (f64, f64),
    /// The most room the surface may take.
    pub avail: (f64, f64),
    pub metrics: Metrics,
    /// Moved when the font configuration changes, so text is measured
    /// again.
    pub font_rev: u64,
    /// Moved when the glyph atlas is rebuilt: kept glyph instances
    /// point into the old one.
    pub atlas_gen: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub build: u64,
    pub layout: u64,
    pub paint: u64,
}

pub struct Surface<O: PaintOut> {
    tree: Tree,
    layout: Layout,
    pub interaction: Interaction,
    out: O,
    counters: Counters,
    metrics: Metrics,
    seen: Option<Inputs>,
    /// The interaction revision and atlas generation the picture last
    /// painted was painted for; `None` when the layout moved since.
    painted: Option<(u64, u64)>,
}

impl<O: PaintOut + Default> Default for Surface<O> {
    fn default() -> Self {
        Self::new(O::default())
    }
}

impl<O: PaintOut> Surface<O> {
    pub fn new(out: O) -> Self {
        Self {
            tree: Tree::new(),
            layout: Layout::new(),
            interaction: Interaction::default(),
            out,
            counters: Counters::default(),
            metrics: Metrics { px_per_pt: 1.0, cell_w: 0.0, cell_h: 0.0, cell_ascent: 0.0 },
            seen: None,
            painted: None,
        }
    }

    /// Bring the kept output up to date with `inputs` and return it.
    /// `describe` is called only when the data or the theme moved.
    pub fn frame(
        &mut self,
        inputs: &Inputs,
        measure: &dyn TextMeasure,
        describe: impl FnOnce(&mut Ui<'_>),
    ) -> &O {
        if self.prepare(inputs, measure, describe) {
            self.paint();
        }
        &self.out
    }

    /// The first two stages, as far as `inputs` say they are stale.
    /// True when the picture last painted no longer shows the surface
    /// and one of the paint stages has to run.
    pub fn prepare(
        &mut self,
        inputs: &Inputs,
        measure: &dyn TextMeasure,
        describe: impl FnOnce(&mut Ui<'_>),
    ) -> bool {
        let was = self.seen;
        let stale_tree = was.is_none_or(|w| (w.data_rev, w.theme_rev) != (inputs.data_rev, inputs.theme_rev));
        if stale_tree {
            self.describe(describe);
        }
        let stale_layout = stale_tree
            || was.is_none_or(|w| {
                (w.origin, w.avail, w.metrics, w.font_rev)
                    != (inputs.origin, inputs.avail, inputs.metrics, inputs.font_rev)
            });
        if stale_layout {
            self.lay_out(inputs, measure);
            self.painted = None;
        }
        self.seen = Some(*inputs);
        self.painted != Some((self.interaction.revision(), inputs.atlas_gen))
    }

    /// Stage one: write the tree again.
    pub fn describe(&mut self, describe: impl FnOnce(&mut Ui<'_>)) {
        self.tree.build(describe);
        self.counters.build += 1;
    }

    /// Stage two: a rectangle for every node.
    pub fn lay_out(&mut self, inputs: &Inputs, measure: &dyn TextMeasure) {
        self.metrics = inputs.metrics;
        let c = Constraints::loose(inputs.avail.0, inputs.avail.1);
        self.layout.run(&self.tree, inputs.origin, c, &inputs.metrics, measure);
        self.interaction.clamp_scroll(&self.layout);
        self.counters.layout += 1;
    }

    /// Stage three: the output's picture, from the tree, the layout
    /// and the interaction state.
    pub fn paint(&mut self) {
        paint(&self.tree, &self.layout, &self.interaction, &self.metrics, &mut self.out);
        self.painted_now();
    }

    /// Stage three into an output the caller holds only for this
    /// call: one that writes through a font and an atlas borrowed for
    /// the frame.  What it wrote is the caller's to keep.
    pub fn paint_into<P: PaintOut>(&mut self, out: &mut P) {
        paint(&self.tree, &self.layout, &self.interaction, &self.metrics, out);
        self.painted_now();
    }

    fn painted_now(&mut self) {
        self.counters.paint += 1;
        self.painted = self.seen.map(|s| (self.interaction.revision(), s.atlas_gen));
    }

    /// The widget at `p`, without changing what is hovered or held.
    pub fn hit(&self, p: (f64, f64)) -> Option<Hit> {
        hit_test(&self.tree, &self.layout, &self.interaction.scroll, self.interaction.offset, p)
    }

    pub fn output(&self) -> &O {
        &self.out
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn pointer_move(&mut self, p: (f64, f64)) -> bool {
        self.interaction.pointer_move(&self.tree, &self.layout, p)
    }

    pub fn pointer_leave(&mut self) -> bool {
        self.interaction.pointer_leave()
    }

    pub fn pointer_down(&mut self, p: (f64, f64)) {
        self.interaction.pointer_down(&self.tree, &self.layout, p);
    }

    pub fn pointer_up(&mut self, p: (f64, f64)) -> Option<Action> {
        self.interaction.pointer_up(&self.tree, &self.layout, p)
    }

    pub fn wheel(&mut self, p: (f64, f64), dy: f64) -> bool {
        self.interaction.wheel(&self.layout, p, dy)
    }

    pub fn key(&mut self, key: Key) -> KeyResult {
        self.interaction.key(&self.tree, &self.layout, key)
    }

    pub fn set_offset(&mut self, offset: (f64, f64)) {
        self.interaction.set_offset(offset);
    }
}
