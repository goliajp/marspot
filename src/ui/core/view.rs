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

use marspot_term::layout::{Rect, Alignment};
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
    /// F3+3.4 — uniform inset from view rect edge to content area
    /// (physical px).  `View::content_rect()` returns `rect.inset(padding)`;
    /// children layout inside that.  Default 0 = content fills view.
    pub padding: f64,
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
            padding: 0.0,
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
    /// Convenience constructor.  Pairs with `Default::default()` for
    /// ViewStyle when the caller wants opaque defaults.
    pub fn new(rect: Rect, style: ViewStyle) -> Self {
        Self { rect, style }
    }

    /// F3+3.4 — inner rect for child content.  Equals
    /// `self.rect.inset(self.style.padding)`.  Use this for laying
    /// out children rather than `self.rect` so the view's padding
    /// is honoured automatically.  See `Rect::inset`.
    pub fn content_rect(&self) -> Rect {
        self.rect.inset(self.style.padding)
    }

    /// Translate a rect from "relative to this view's top-left" to
    /// absolute screen coords.  Mirrors React Native's child-position
    /// convention: caller treats the parent's top-left as origin.
    ///
    /// ```ignore
    /// let parent = View::new(absolute_rect, parent_style);
    /// parent.paint(p, |p| {
    ///     // Child at (10, 10) inside parent, 200×50.
    ///     let child = parent.child(
    ///         Rect { x: 10.0, y_top: 10.0, w: 200.0, h: 50.0 },
    ///         child_style,
    ///     );
    ///     child.paint(p, |p| {
    ///         p.text(child.rect.x as f32,
    ///                child.rect.y_top as f32 + p.ascent,
    ///                "nested", FG);
    ///     });
    /// });
    /// ```
    ///
    /// Nesting is purely additive — every child painted inside the
    /// parent's body closure lands on top in z-order (push order).
    /// No automatic clipping: a child that paints past the parent
    /// rect will still draw.  Treat parent rect as design intent.
    pub fn relative_rect(&self, offset: Rect) -> Rect {
        Rect {
            x: self.rect.x + offset.x,
            y_top: self.rect.y_top + offset.y_top,
            w: offset.w,
            h: offset.h,
        }
    }

    /// Build a nested child view at `offset` relative to this view's
    /// top-left.  Shortcut for `View::new(self.relative_rect(offset), style)`.
    pub fn child(&self, offset: Rect, style: ViewStyle) -> View {
        View::new(self.relative_rect(offset), style)
    }

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

    /// Proportional text in the **system UI font** (SF Pro), at the
    /// system UI point size — the same font and size the native macOS
    /// title bar and section headers use.  `text()` above renders the
    /// terminal mono font at the grid's cell size, which is a
    /// *different, smaller* face; a modal heading drawn with it reads
    /// as visibly smaller than the surrounding system chrome.  Use this
    /// for headings that should sit at parity with the OS, and keep
    /// `text()` for tabular / numeric content that wants monospace.
    ///
    /// `baseline_y` is the text baseline in physical px.  Weight 600
    /// (semibold) matches the macOS title convention.
    pub fn ui_text(&mut self, x: f32, baseline_y: f32, s: &str, color: [f32; 4]) {
        crate::render_metal::push_text_run_ui_shaped_mono(
            s, x, baseline_y, color,
            self.ascent, self.atlas_w, self.atlas_h,
            600,
            self.font, self.atlas, self.glyphs,
        );
    }

    /// A run in one of the panel type roles — the size ladder every
    /// panel shares.  See [`crate::ui::theme::PanelText`].
    ///
    /// Prefer this over [`Self::ui_text_at`] with numbers: a panel
    /// that names a size is a panel that will disagree with the next
    /// one, which is how the settings panel came to set its row labels
    /// at the size of every other panel's title.
    pub fn panel_text(
        &mut self,
        role: crate::ui::theme::PanelText,
        x: f32,
        baseline_y: f32,
        s: &str,
        color: [f32; 4],
    ) {
        self.ui_text_at(x, baseline_y, s, role.pt(), role.weight(), color);
    }

    /// Width of `s` in `role`, physical px.
    pub fn panel_text_width(
        &mut self,
        role: crate::ui::theme::PanelText,
        s: &str,
    ) -> f32 {
        self.ui_text_width_at(s, role.pt(), role.weight())
    }

    /// Anchor-aligned text in a panel role — the proportional
    /// counterpart to [`Self::text_in`].
    ///
    /// `text_in` sizes its box as `chars * cell_w`, which is only true
    /// for the mono cell font; using it for a proportional run puts a
    /// centred title off-centre by however much the string's real
    /// width differs from its character count.  This measures.
    pub fn panel_text_in(
        &mut self,
        role: crate::ui::theme::PanelText,
        rect: Rect,
        s: &str,
        color: [f32; 4],
        align: Alignment,
    ) {
        let w = self.panel_text_width(role, s) as f64;
        let cap = (crate::ui::view::type_scale::sf_pro_cap_height(role.pt())
            * Self::px_per_pt()) as f64;
        let box_rect = rect.place(w, cap, align);
        // `place` gave the cap box; the baseline is its bottom.
        self.panel_text(role, box_rect.x as f32, (box_rect.y_top + cap) as f32, s, color);
    }

    /// The system UI font at an explicit **pt size and weight**.
    ///
    /// `ui_text` above is one size (the startup chrome pt) at one
    /// weight (600).  A surface built only from it has no type scale:
    /// a label and its explanatory line come out the same size and can
    /// differ only in colour, which is not a hierarchy — it is two
    /// equal lines, one of them harder to read.  This is the sized
    /// path the canvas system has had since Phase 10c, exposed to the
    /// direct painter.
    ///
    /// `baseline_y` is a real baseline in physical px; see
    /// [`Self::ui_baseline_centred`] for putting one in the middle of
    /// a box.
    pub fn ui_text_at(
        &mut self,
        x: f32,
        baseline_y: f32,
        s: &str,
        size_pt: f64,
        weight: u16,
        color: [f32; 4],
    ) {
        crate::render_metal::push_text_run_ui_sized(
            s, x, baseline_y, color, size_pt, weight,
            self.atlas_w, self.atlas_h,
            self.font, self.atlas, self.glyphs,
        );
    }

    /// Advance width of `s` at an explicit pt size and weight,
    /// physical px.  Pairs with [`Self::ui_text_at`] — measuring at a
    /// different size than you draw is how a label overruns the button
    /// drawn to hold it.
    pub fn ui_text_width_at(&mut self, s: &str, size_pt: f64, weight: u16) -> f32 {
        self.font.measure_ui_text_at_size(
            s, weight, crate::font_shape::ShapeOptions::default(), size_pt,
        ) as f32
    }

    /// Baseline that optically centres `size_pt` text in a box whose
    /// top and height are given, in physical px.
    ///
    /// Centres on **cap height**, not the em box — see
    /// [`crate::ui::view::type_scale::sf_pro_cap_height`].
    pub fn ui_baseline_centred(&self, box_top: f32, box_h: f32, size_pt: f64) -> f32 {
        let cap = (crate::ui::view::type_scale::sf_pro_cap_height(size_pt)
            * Self::px_per_pt()) as f32;
        box_top + (box_h + cap) * 0.5
    }

    /// Physical pixels per point, for panels laid out in points.
    ///
    /// Two parts: the atlas rasterises at 2×, which is the unit the
    /// panel constants were written in, and the display's own scale on
    /// top of that (`marspot::ui::chrome_scale`).  At `scale == 1` this
    /// is the 2.0 it has always been.
    pub fn px_per_pt() -> f64 {
        2.0 * crate::ui::chrome_scale()
    }

    /// Advance width of `s` in the system UI font, physical px — pairs
    /// with `ui_text` for right-aligning or centring a heading.
    pub fn ui_text_width(&mut self, s: &str) -> f32 {
        self.font.measure_ui_text(
            s, 600, crate::font_shape::ShapeOptions::default(),
        ) as f32
    }

    /// Ascent of the system UI font in physical px — for turning a
    /// top-of-box y into a `ui_text` baseline.
    pub fn ui_ascent(&self) -> f32 {
        self.font.ui_ascent as f32
    }

    /// Cell height of the system UI font in physical px.
    pub fn ui_line_h(&self) -> f32 {
        self.font.ui_cell_h as f32
    }

    /// F3+3.4 — anchor-aligned text inside a rect.  Text width is
    /// `s.chars().count() * cell_w` (assumes monospace, fine for
    /// chrome labels in marspot); text height is `cell_h`.  Caller
    /// picks the anchor — `Alignment::CenterLeft` for "left-padded
    /// vertical center", `Alignment::Center` for fully centered,
    /// `Alignment::CenterRight` for right-aligned, etc.
    ///
    /// Internally calls `Rect::place` to compute the text box, then
    /// `text()` at the resulting top-left + ascent baseline.
    /// Saves chrome paint sites from doing the `(w - text_w) * 0.5`
    /// math by hand (and getting it inconsistent / off-by-one).
    pub fn text_in(
        &mut self,
        rect: Rect,
        s: &str,
        color: [f32; 4],
        align: Alignment,
    ) {
        let cell_w = self.cell_w;
        let cell_h = self.cell_h;
        let ascent = self.ascent;
        let text_w = s.chars().count() as f32 * cell_w;
        let box_rect = rect.place(text_w as f64, cell_h as f64, align);
        let baseline_y = box_rect.y_top as f32 + ascent;
        self.text(box_rect.x as f32, baseline_y, s, color);
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

    #[test]
    fn relative_rect_translates_offset_by_parent_origin() {
        let parent = View::new(
            Rect { x: 100.0, y_top: 50.0, w: 800.0, h: 600.0 },
            ViewStyle::default(),
        );
        let child_offset = Rect { x: 20.0, y_top: 10.0, w: 200.0, h: 50.0 };
        let child_abs = parent.relative_rect(child_offset);
        assert_eq!(child_abs.x, 120.0);
        assert_eq!(child_abs.y_top, 60.0);
        assert_eq!(child_abs.w, 200.0);
        assert_eq!(child_abs.h, 50.0);
    }

    #[test]
    fn child_builds_view_at_relative_offset() {
        let parent = View::new(
            Rect { x: 10.0, y_top: 10.0, w: 100.0, h: 100.0 },
            ViewStyle::default(),
        );
        let c = parent.child(
            Rect { x: 5.0, y_top: 5.0, w: 20.0, h: 20.0 },
            ViewStyle::default(),
        );
        assert_eq!(c.rect.x, 15.0);
        assert_eq!(c.rect.y_top, 15.0);
    }

    #[test]
    fn nesting_is_associative_two_levels_deep() {
        // grandparent → parent (offset 10, 10) → child (offset 5, 5)
        // should end at gp.origin + 15, +15.
        let gp = View::new(
            Rect { x: 100.0, y_top: 50.0, w: 800.0, h: 600.0 },
            ViewStyle::default(),
        );
        let parent = gp.child(
            Rect { x: 10.0, y_top: 10.0, w: 400.0, h: 300.0 },
            ViewStyle::default(),
        );
        let child = parent.child(
            Rect { x: 5.0, y_top: 5.0, w: 200.0, h: 100.0 },
            ViewStyle::default(),
        );
        assert_eq!(child.rect.x, 115.0);
        assert_eq!(child.rect.y_top, 65.0);
    }
}
