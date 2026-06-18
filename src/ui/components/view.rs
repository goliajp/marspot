//! `View` — base component for any pixel-correct overlay surface in
//! marspot.  Wraps the "overlay scratch + extra render pass" plumbing
//! so callers never deal with which pipeline / which scratch / how
//! z-order works.  Configure frame + style + (optional) backdrop,
//! paint your content via the supplied `ViewPainter`, done.
//!
//! ## Why a View
//!
//! Metal's pipeline order (BG cells → DOT → UI rects → FG glyphs) is
//! a single sequence per frame.  Anything in a later pass paints over
//! anything in an earlier pass — there's no concept of "overlay vs
//! grid" at the pipeline level.  Marspot's renderer adds three extra
//! passes (BG → UI → FG) that consume dedicated `overlay_*` scratch
//! vecs and run AFTER the main four passes; everything pushed to
//! overlay scratches is on top of every grid pixel, period.
//!
//! Forcing every overlay-bearing feature to learn that pattern is
//! how the "panel is still transparent" bug recurred: each call site
//! had to remember to route to overlay scratches, filter grid glyphs,
//! pick the right pipeline.  `View` makes the right thing
//! automatic — call sites only see a painter that draws above
//! everything, by construction.
//!
//! ## API shape
//!
//! ```ignore
//! let view = View {
//!     rect: modal_rect,
//!     style: ViewStyle { backdrop: Backdrop::Dim { ... }, .. ViewStyle::default() },
//! };
//! view.paint(&mut painter, |p| {
//!     p.fill_rect(title_bar_rect, TITLE_BG);
//!     p.text(title_x, baseline_y, "Process Monitor", TITLE_FG);
//!     // ... rest of the modal content
//! });
//! ```

use marspot_term::layout::Rect;
use crate::render_metal::{CellInstance, GlyphInstance, UiRectInstance};
use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;

/// Visual style for a `View`.  All colors are RGBA; alpha **should be
/// 1.0** (opaque) for any user-facing surface — semi-transparent
/// panels read as "broken" in this app, not "elegant".  Defaults
/// match marspot's existing panel chrome.
#[derive(Debug, Clone, Copy)]
pub struct ViewStyle {
    pub bg: [f32; 4],
    pub border_color: [f32; 4],
    pub border_width: f32,
    pub corner_radius: f32,
    pub shadow_blur: f32,
    pub shadow_alpha: f32,
    pub backdrop: Backdrop,
}

/// Optional "behind-the-view" dimmer.  Painted in overlay scratches
/// BEFORE the view's own frame so the view always covers it.  Use
/// `exclude_above_y` to keep the marspot title strip undimmed
/// (per the project's "title bar always on top" invariant).
#[derive(Debug, Clone, Copy)]
pub enum Backdrop {
    None,
    Dim { color: [f32; 4], exclude_above_y: f64 },
}

impl Default for ViewStyle {
    fn default() -> Self {
        Self {
            bg:          [0.13, 0.14, 0.17, 1.0],
            border_color:[0.32, 0.34, 0.40, 1.0],
            border_width: 1.0,
            corner_radius: 10.0,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            backdrop: Backdrop::None,
        }
    }
}

/// A pixel-correct overlay surface positioned at `rect` with the
/// given `style`.  Drawing into a View uses overlay scratches; the
/// surface is guaranteed to render above all grid content.
#[derive(Debug, Clone, Copy)]
pub struct View {
    pub rect: Rect,
    pub style: ViewStyle,
}

impl View {
    /// Push the View's backdrop (if any) + chrome (shadow / BG /
    /// border) into the painter's overlay scratches, then call
    /// `body` so the caller paints their content into the same
    /// scratches.  Pipeline ordering guarantees:
    ///   1. backdrop UI rect (semi-trans dim)
    ///   2. View frame UI rect (opaque BG + border + shadow)
    ///   3. caller's content (rects, rounded rects, glyphs)
    /// All three live in overlay scratches, so they render after every
    /// main grid pass.  Caller text / fills inside the View frame
    /// always sit on top of the View BG.
    pub fn paint<F>(&self, painter: &mut ViewPainter, body: F)
    where
        F: FnOnce(&mut ViewPainter),
    {
        // 1. Backdrop.
        if let Backdrop::Dim { color, exclude_above_y } = self.style.backdrop {
            let y_top = exclude_above_y as f32;
            painter.ui_rects.push(UiRectInstance {
                origin: [0.0, y_top],
                size: [
                    painter.window_w as f32,
                    (painter.window_h as f32 - y_top).max(0.0),
                ],
                fill_color: color,
                border_color: [0.0, 0.0, 0.0, 0.0],
                corner_radius: 0.0,
                border_width: 0.0,
                shadow_blur: 0.0,
                shadow_alpha: 0.0,
                shadow_color: [0.0, 0.0, 0.0, 1.0],
            });
        }
        // 2. Frame (one SDF rect = BG + border + shadow).
        painter.ui_rects.push(UiRectInstance {
            origin: [self.rect.x as f32, self.rect.y_top as f32],
            size: [self.rect.w as f32, self.rect.h as f32],
            fill_color: self.style.bg,
            border_color: self.style.border_color,
            corner_radius: self.style.corner_radius,
            border_width: self.style.border_width,
            shadow_blur: self.style.shadow_blur,
            shadow_alpha: self.style.shadow_alpha,
            shadow_color: [0.0, 0.0, 0.0, 1.0],
        });
        // 3. Caller content.
        body(painter);
    }
}

/// Drawing context handed to a View's body callback.  Routes every
/// primitive to the renderer's overlay scratches.  Caller never sees
/// — and can't accidentally bypass — that fact.
pub struct ViewPainter<'a> {
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    pub atlas_w: f32,
    pub atlas_h: f32,
    pub window_w: f64,
    pub window_h: f64,
    pub font: &'a mut FontCache,
    pub atlas: &'a mut GlyphAtlas,
    pub cells: &'a mut Vec<CellInstance>,
    pub glyphs: &'a mut Vec<GlyphInstance>,
    pub ui_rects: &'a mut Vec<UiRectInstance>,
}

impl<'a> ViewPainter<'a> {
    /// Axis-aligned solid fill via the BG (cells) pipeline.  No anti-
    /// alias on corners; use `fill_rounded_rect` for that.
    pub fn fill_rect(&mut self, rect: Rect, color: [f32; 4]) {
        self.cells.push(CellInstance {
            origin: [rect.x as f32, rect.y_top as f32],
            size:   [rect.w as f32, rect.h as f32],
            color,
        });
    }

    /// SDF-anti-aliased rounded rectangle.  Pass `border = ([0;4], 0.0)`
    /// for borderless.  Pass `radius = 0.0` for a sharp rect (still
    /// anti-aliased on edges, unlike `fill_rect`).
    pub fn fill_rounded_rect(
        &mut self,
        rect: Rect,
        color: [f32; 4],
        radius: f32,
        border: ([f32; 4], f32),
    ) {
        self.ui_rects.push(UiRectInstance {
            origin: [rect.x as f32, rect.y_top as f32],
            size:   [rect.w as f32, rect.h as f32],
            fill_color: color,
            border_color: border.0,
            corner_radius: radius,
            border_width: border.1,
            shadow_blur: 0.0,
            shadow_alpha: 0.0,
            shadow_color: [0.0, 0.0, 0.0, 1.0],
        });
    }

    /// Text run.  `x` and `baseline_y` are in physical pixels.
    /// Glyph metrics come from the painter's font cache; missing
    /// glyphs are rasterised into the painter's atlas on demand
    /// (atlas rebuilds during a paint frame are safe — the renderer
    /// catches the new generation count after `build_instances`).
    pub fn text(&mut self, x: f32, baseline_y: f32, s: &str, color: [f32; 4]) {
        crate::render_metal::push_text_run(
            s, x, baseline_y, color,
            self.cell_w, self.cell_h, self.ascent,
            self.atlas_w, self.atlas_h,
            self.font, self.atlas, self.glyphs,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_style_is_opaque() {
        let s = ViewStyle::default();
        assert_eq!(s.bg[3], 1.0, "View BG must be opaque by default");
        assert_eq!(s.border_color[3], 1.0, "border must be opaque by default");
    }
}
