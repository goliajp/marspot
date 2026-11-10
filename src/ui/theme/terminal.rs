//! The colours drawn inside and behind a pane's grid, in the shape
//! the per-cell paths read them.
//!
//! They are the theme's like any other colour.  The code that uses
//! them runs per cell and per glyph, where naming a colour has to cost
//! nothing: a frame takes one [`TermColors`] when its build starts and
//! passes it down, so the per-cell path reads plain fields and never
//! asks which theme is in force.

use super::table::{self, Theme};
use crate::ui::core::Color;
use golia_ui_core::Rgba8;
use marspot_term::grid::{CellAttrs, Color as CellColor, ColorKind};
use marspot_term::palette::{Palette, ThemeId};

const fn rgba8(c: Color) -> Rgba8 {
    Rgba8::rgba(c.r, c.g, c.b, 255)
}

/// One frame's terminal colours.
#[derive(Clone, Copy)]
pub(crate) struct TermColors {
    /// Part of what a pane's cached instances depend on.
    pub(crate) id: ThemeId,
    pal: &'static Palette,
    dim_towards_bg: bool,
    /// Behind a search hit; the glyph keeps its own colour on top.
    pub(crate) search_hit: Rgba8,
    /// Behind selected cells.
    pub(crate) selection: Rgba8,
    /// An IME composition that has not been committed yet.
    pub(crate) ime_bg: Rgba8,
    pub(crate) ime_fg: Rgba8,
    /// The cursor: a block in the pane being typed into, an outline
    /// elsewhere.
    pub(crate) cursor: [f32; 4],
    /// A detected link: its underline and its text.
    pub(crate) link: (f64, f64, f64),
    /// The glyph under a solid cursor.
    pub(crate) bg: Rgba8,
}

impl TermColors {
    pub(crate) fn of(t: &'static Theme) -> Self {
        let unit = |c: Color| (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
        let cursor = unit(t.cursor_text);
        let link = unit(t.link);
        let bg = t.term.bg;
        Self {
            id: t.id,
            pal: t.term,
            dim_towards_bg: bg.1 > 0.5,
            search_hit: rgba8(t.search_hit),
            selection: rgba8(t.selection),
            ime_bg: rgba8(t.ime_bg),
            ime_fg: rgba8(t.ime_fg),
            cursor: [cursor.0, cursor.1, cursor.2, 1.0],
            link: (link.0 as f64, link.1 as f64, link.2 as f64),
            bg: crate::frame_build::color::rgba8_of_f32([bg.0 as f32, bg.1 as f32, bg.2 as f32, 1.0]),
        }
    }

    /// The theme in force.  Once per frame, not per cell.
    pub(crate) fn current() -> Self {
        Self::of(table::current())
    }

    /// The terminal's default background: a cell of this colour is not
    /// drawn, the pane's own fill shows through.
    pub(crate) fn default_bg(&self) -> (f64, f64, f64) {
        self.pal.bg
    }

    fn resolve(&self, c: CellColor, default_rgb: (f64, f64, f64)) -> (f64, f64, f64) {
        match c.kind() {
            ColorKind::Default => default_rgb,
            ColorKind::Indexed(i) => self.pal.indexed(i),
            ColorKind::Rgb(r, g, b) => (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0),
        }
    }

    /// A cell's attrs as (fg, bg) RGB, honouring SGR reverse + dim.
    pub(crate) fn resolve_attrs(&self, attrs: CellAttrs) -> ((f64, f64, f64), (f64, f64, f64)) {
        let mut fg = self.resolve(attrs.fg, self.pal.fg);
        let mut bg = self.resolve(attrs.bg, self.pal.bg);
        if attrs.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if attrs.dim {
            // SGR 2 — half-intensity. xterm-style multiply by ~0.55 in
            // sRGB; close enough to "secondary text" weight without
            // tinting hues. Applied AFTER reverse so a reversed-dim cell
            // still reads correctly (rare combo but spec-clean).
            const DIM: f64 = 0.55;
            fg = if self.dim_towards_bg {
                // On a light ground a darker foreground is a stronger
                // one; fading means moving towards the paper.
                let b = self.pal.bg;
                (b.0 + (fg.0 - b.0) * DIM, b.1 + (fg.1 - b.1) * DIM, b.2 + (fg.2 - b.2) * DIM)
            } else {
                (fg.0 * DIM, fg.1 * DIM, fg.2 * DIM)
            };
        }
        (fg, bg)
    }
}
