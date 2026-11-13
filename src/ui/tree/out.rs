//! A painted tree, written into the instance buffers a frame is made
//! of.
//!
//! The tree's painter calls this with rectangles, runs of text and
//! presets; each becomes what the panels' own painters would have
//! pushed for the same thing, through the same [`ViewPainter`], so a
//! surface described as a tree and one painted by hand share one
//! picture format and one glyph atlas.

use super::{exact_pt, preset, tone};
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::{self, focus};
use golia_ui_core::{Ink, PaintOut, Rect, TextFont, WidgetState};
use marspot_term::layout::Rect as PxRect;

/// Clips nested deeper than this are not something a panel has.
const CLIP_DEPTH: usize = 8;

/// Draws the nodes a panel draws itself: the tag and data of one of
/// them, and the painter to draw it with.
pub type Custom<'c> = dyn FnMut(&mut ViewPainter<'_>, u32, [u32; 2]) + 'c;

pub struct TreeOut<'p, 'a, 'c> {
    p: &'p mut ViewPainter<'a>,
    /// What draws a node tagged `preset::CUSTOM` or above.
    custom: Option<&'p mut Custom<'c>>,
    /// How long the three buffers were when this output was made:
    /// what `reset` takes them back to.
    start: (usize, usize, usize),
    clips: [Rect; CLIP_DEPTH],
    depth: usize,
}

fn px_rect(r: Rect) -> PxRect {
    PxRect { x: r.x, y_top: r.y, w: r.w, h: r.h }
}

impl<'p, 'a, 'c> TreeOut<'p, 'a, 'c> {
    pub fn new(p: &'p mut ViewPainter<'a>) -> Self {
        let start = (p.cells.len(), p.glyphs.len(), p.ui_rects.len());
        Self { p, custom: None, start, clips: [Rect::default(); CLIP_DEPTH], depth: 0 }
    }

    /// An output for a tree with nodes its panel draws itself.
    pub fn with_custom(p: &'p mut ViewPainter<'a>, custom: &'p mut Custom<'c>) -> Self {
        Self { custom: Some(custom), ..Self::new(p) }
    }

    fn clip(&self) -> Option<Rect> {
        self.depth.checked_sub(1).map(|top| self.clips[top])
    }

    /// Whether any of `rect` is inside the clip in force.
    fn shows(&self, rect: &Rect) -> bool {
        self.clip().is_none_or(|c| !c.intersect(rect).is_empty())
    }
}

impl PaintOut for TreeOut<'_, '_, '_> {
    fn reset(&mut self) {
        self.p.cells.truncate(self.start.0);
        self.p.glyphs.truncate(self.start.1);
        self.p.ui_rects.truncate(self.start.2);
        self.depth = 0;
    }

    fn solid_rect(&mut self, rect: Rect, ink: Ink) {
        let rect = self.clip().map_or(rect, |c| c.intersect(&rect));
        if !rect.is_empty() {
            self.p.fill_rect(px_rect(rect), tone::rgba(ink));
        }
    }

    // a rounded rectangle and a glyph carry no scissor of their own,
    // so under a clip each is drawn whole or not at all: the rule the
    // panels' scrolling already follows
    fn rounded_rect(&mut self, rect: Rect, radius: f64, fill: Option<Ink>, border: Option<(f64, Ink)>) {
        if !self.shows(&rect) {
            return;
        }
        let fill = fill.map_or(palette::none(), tone::rgba);
        let border = border.map_or((palette::none(), 0.0), |(w, ink)| (tone::rgba(ink), w as f32));
        self.p.fill_rounded_rect(px_rect(rect), fill, radius as f32, border);
    }

    fn text_run(&mut self, x: f64, baseline: f64, text: &str, font: TextFont, ink: Ink) {
        if self.clip().is_some_and(|c| baseline < c.y || baseline > c.bottom() || x >= c.right()) {
            return;
        }
        let color = tone::rgba(ink);
        match font {
            TextFont::Ui(spec) => {
                self.p.ui_text_at(x as f32, baseline as f32, text, exact_pt(spec), spec.weight, color)
            }
            TextFont::Mono => self.p.text(x as f32, baseline as f32, text, color),
        }
    }

    fn leaf(&mut self, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState) {
        if !self.shows(&rect) {
            return;
        }
        if tag < preset::CUSTOM {
            preset::paint(self.p, tag, data, label, px_rect(rect), state);
        } else if let Some(custom) = self.custom.as_mut() {
            custom(self.p, tag, data);
        }
    }

    // a stroke round the widget, clear of it, with nothing inside
    fn focus_ring(&mut self, rect: Rect, radius: f64) {
        let grow = spec::px(focus::RING_OUTSET + focus::RING_W);
        let r = spec::snap_rect(px_rect(rect));
        let ring = PxRect { x: r.x - grow, y_top: r.y_top - grow, w: r.w + 2.0 * grow, h: r.h + 2.0 * grow };
        let stroke = (palette::accent(), spec::px(focus::RING_W) as f32);
        self.p.fill_rounded_rect(ring, palette::none(), (radius + grow) as f32, stroke);
    }

    fn push_clip(&mut self, rect: Rect) {
        self.clips[self.depth] = self.clip().map_or(rect, |c| c.intersect(&rect));
        self.depth += 1;
    }

    fn pop_clip(&mut self) {
        self.depth -= 1;
    }
}
