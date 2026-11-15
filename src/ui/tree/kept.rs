//! A surface together with the instances it last painted.
//!
//! A frame in which nothing the surface shows has changed describes
//! nothing, lays nothing out and paints nothing: the kept instances
//! are copied behind whatever the frame already holds.  Their buffers
//! are cleared, never freed, so a repaint of a picture the same size
//! takes nothing from the heap either.

use super::out::Custom;
use super::{FontMeasure, TreeOut, metrics};
use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::core::ViewPainter;
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};
use golia_ui_core::{Inputs, NoOut, Surface, Ui};

/// What moves when glyph instances kept from an earlier frame may no
/// longer point at their glyphs: the atlas was emptied, or one of its
/// shelves was handed to other glyphs.
pub fn atlas_generation(atlas: &GlyphAtlas) -> u64 {
    atlas.rebuild_count + atlas.evict_count
}

/// What moves when the words or the colours of the interface do.
pub fn theme_revision() -> u64 {
    ((marspot_term::palette::version() as u64) << 32) | marspot_term::language::version() as u64
}

/// Where a surface is and which revision of its data it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    /// Moved by the caller whenever what the surface shows changed.
    pub data_rev: u64,
    /// Top-left, device pixels.
    pub origin: (f64, f64),
    /// The most room it may take.
    pub avail: (f64, f64),
}

#[derive(Default)]
pub struct KeptSurface {
    pub surface: Surface<NoOut>,
    cells: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    ui_rects: Vec<UiRectInstance>,
    /// The atlas generation of the last frame painted.
    atlas_gen: u64,
}

impl KeptSurface {
    pub fn new() -> Self {
        Self::default()
    }

    fn inputs(&self, font: &FontCache, at: Place) -> Inputs {
        Inputs {
            data_rev: at.data_rev,
            theme_rev: theme_revision(),
            origin: at.origin,
            avail: at.avail,
            metrics: metrics(font),
            font_rev: 0,
            atlas_gen: self.atlas_gen,
        }
    }

    /// Describe and lay out as far as `at` says is stale, and paint
    /// nothing: for input that arrives between frames and has to be
    /// tested against the layout the next frame will be painted from.
    pub fn lay_out(
        &mut self,
        font: &mut FontCache,
        at: Place,
        describe: impl FnOnce(&mut Ui<'_>, &FontMeasure<'_>),
    ) {
        let inputs = self.inputs(font, at);
        let m = FontMeasure::new(font);
        self.surface.prepare(&inputs, &m, |ui| describe(ui, &m));
    }

    /// Add the surface to the frame `p` is painting.  `describe` runs
    /// only when the data or the theme moved; it is handed the measure
    /// the layout will use, for controls sized by their labels.
    ///
    /// `pointer` is where the pointer is when nobody has been telling
    /// the surface as it moved: it is looked up in the layout this
    /// frame is painted from.
    pub fn frame(
        &mut self,
        p: &mut ViewPainter<'_>,
        at: Place,
        pointer: Option<(f64, f64)>,
        describe: impl FnOnce(&mut Ui<'_>, &FontMeasure<'_>),
    ) {
        self.frame_with(p, at, pointer, describe, &mut |_: &mut ViewPainter<'_>, _, _| {});
    }

    /// [`Self::frame`] for a tree with nodes its panel draws itself:
    /// `custom` is called for each when the surface is painted.
    pub fn frame_with(
        &mut self,
        p: &mut ViewPainter<'_>,
        at: Place,
        pointer: Option<(f64, f64)>,
        describe: impl FnOnce(&mut Ui<'_>, &FontMeasure<'_>),
        custom: &mut Custom<'_>,
    ) {
        self.refresh_with(p, at, pointer, describe, custom);
        p.cells.extend_from_slice(&self.cells);
        p.glyphs.extend_from_slice(&self.glyphs);
        p.ui_rects.extend_from_slice(&self.ui_rects);
    }

    /// [`Self::frame`] that adds nothing to the frame `p` is painting:
    /// the kept instances are brought up to date and left where they
    /// are, for a surface drawn in a pass of its own.  True when they
    /// were painted again.
    pub fn refresh(
        &mut self,
        p: &mut ViewPainter<'_>,
        at: Place,
        pointer: Option<(f64, f64)>,
        describe: impl FnOnce(&mut Ui<'_>, &FontMeasure<'_>),
    ) -> bool {
        self.refresh_with(p, at, pointer, describe, &mut |_: &mut ViewPainter<'_>, _, _| {})
    }

    fn refresh_with(
        &mut self,
        p: &mut ViewPainter<'_>,
        at: Place,
        pointer: Option<(f64, f64)>,
        describe: impl FnOnce(&mut Ui<'_>, &FontMeasure<'_>),
        custom: &mut Custom<'_>,
    ) -> bool {
        self.atlas_gen = atlas_generation(p.atlas);
        let inputs = self.inputs(p.font, at);
        let mut stale = {
            let m = FontMeasure::new(&mut *p.font);
            self.surface.prepare(&inputs, &m, |ui| describe(ui, &m))
        };
        if let Some(at) = pointer {
            stale |= self.surface.pointer_move(at);
        }
        if stale {
            self.cells.clear();
            self.glyphs.clear();
            self.ui_rects.clear();
            let mut kept = ViewPainter {
                cell_w: p.cell_w,
                cell_h: p.cell_h,
                ascent: p.ascent,
                atlas_w: p.atlas_w,
                atlas_h: p.atlas_h,
                window_w: p.window_w,
                window_h: p.window_h,
                font: &mut *p.font,
                atlas: &mut *p.atlas,
                cells: &mut self.cells,
                glyphs: &mut self.glyphs,
                ui_rects: &mut self.ui_rects,
            };
            self.surface.paint_into(&mut TreeOut::with_custom(&mut kept, custom));
        }
        stale
    }

    /// The instances kept: plain rectangles, glyphs, rounded
    /// rectangles.
    pub fn kept(&self) -> (&[RectInstance], &[GlyphInstance], &[UiRectInstance]) {
        (&self.cells, &self.glyphs, &self.ui_rects)
    }

    /// Instances kept: `(plain rectangles, glyphs, rounded rectangles)`.
    pub fn kept_len(&self) -> (usize, usize, usize) {
        (self.cells.len(), self.glyphs.len(), self.ui_rects.len())
    }
}
