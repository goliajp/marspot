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

#[derive(Clone, Copy, PartialEq)]
struct Seen {
    inputs: Inputs,
    interaction: u64,
}

pub struct Surface<O: PaintOut> {
    tree: Tree,
    layout: Layout,
    pub interaction: Interaction,
    out: O,
    counters: Counters,
    metrics: Metrics,
    seen: Option<Seen>,
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
        let was = self.seen.map(|s| s.inputs);
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
        }
        let stale_paint = stale_layout
            || self.seen.is_none_or(|s| {
                s.interaction != self.interaction.revision() || s.inputs.atlas_gen != inputs.atlas_gen
            });
        if stale_paint {
            self.paint();
        }
        self.seen = Some(Seen { inputs: *inputs, interaction: self.interaction.revision() });
        &self.out
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
        self.counters.paint += 1;
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
