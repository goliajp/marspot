//! Canvas — the one drawing surface.
//!
//! ## What this is
//!
//! `Canvas` is the public drawing API every UI component talks
//! to.  Components call `canvas.rect()` / `canvas.line()` /
//! `canvas.text()`, get a builder, fill in geometry + style, and
//! call `.draw()` to enqueue a primitive.  At flush time, the
//! renderer walks the queue **in submission order** and routes
//! each primitive to the correct Metal pipeline — switching
//! pipelines mid-frame as needed so that **later submission =
//! drawn on top regardless of which pipeline carries it**.
//!
//! Components NEVER pick a pipeline.  Components NEVER call
//! `* scale`.  Components NEVER write `[f32; 4]` colour arrays.
//! Components ONLY work in [`Pt`] / [`Pct`] / [`Length`] /
//! [`Color`].
//!
//! ## Z-order rule
//!
//! ```ignore
//! canvas.rect().at(Pt(0), Pt(0)).size(Pct(1.0), Pct(1.0)).fill(BG).draw();   // z=0
//! canvas.text(Pt(8), Pt(8), "hi").color(FG).draw();                           // z=1
//! canvas.rect().at(Pt(0), Pt(20)).size(Pct(1.0), Pt(1)).fill(LINE).draw();    // z=2
//! ```
//!
//! The line at z=2 will sit on top of the BG at z=0 AND on top
//! of the text at z=1.  The text at z=1 will sit on top of the
//! BG.  This is the single invariant — there is no other rule.
//!
//! ## How resolution happens
//!
//! Builders accept [`Length`] (the sum type).  At `draw()` time,
//! the builder resolves against the Canvas's current parent
//! rect (origin + size in physical pixels) using
//! `Length::resolve_for_axis`.  The resolved primitive carries
//! physical-pixel `f64`s in [`Primitive::Resolved`] form.
//!
//! Components can `.push_clip(rect)` to switch the parent
//! coord-space for nested layouts; not needed for the initial
//! P2 migration.

use super::color::Color;
use super::units::{Length, Pt};

/// Drawing surface.  One per overlay (menu, modal, panel) or
/// per main pass.  Holds a monotonic submission queue and the
/// resolution context (parent rect + device scale).
pub struct Canvas {
    /// Device pixel ratio.  `Pt(1.0)` → `1.0 * scale` physical
    /// pixels.
    pub scale: f64,
    /// Parent rect for `Pct` resolution + drawing origin.
    /// Physical pixels.  Defaults to (0, 0, window_w, window_h)
    /// for top-level canvases.
    pub parent: ParentRect,
    /// Submission queue.  Order = z.
    primitives: Vec<Primitive>,
}

#[derive(Clone, Copy, Debug)]
pub struct ParentRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl ParentRect {
    pub fn window(w_phys: f64, h_phys: f64) -> Self {
        Self { x: 0.0, y: 0.0, w: w_phys, h: h_phys }
    }
}

impl Canvas {
    pub fn new(scale: f64, parent: ParentRect) -> Self {
        Self { scale, parent, primitives: Vec::new() }
    }

    /// Begin a `Rect` builder.  Position via `.at(x, y)`,
    /// size via `.size(w, h)`, optionally `.radius(r)` /
    /// `.border(w, c)` / `.shadow(blur, offset, c)`.  Finalise
    /// via `.fill(color).draw()`.
    pub fn rect(&mut self) -> RectBuilder<'_> {
        RectBuilder {
            canvas: self,
            x: Length::Pt(0.0),
            y: Length::Pt(0.0),
            w: Length::Pct(0.0),
            h: Length::Pct(0.0),
            fill: Color::TRANSPARENT,
            radius: Pt::ZERO,
            border: None,
            shadow: None,
        }
    }

    /// Begin a `Line` builder.  Stroke width via `.stroke(w, c)`.
    pub fn line(&mut self, from: (Length, Length), to: (Length, Length)) -> LineBuilder<'_> {
        LineBuilder {
            canvas: self,
            from,
            to,
            width: Pt(1.0),
            color: Color::TRANSPARENT,
        }
    }

    /// Begin a `Text` builder.  Anchored at top-left of the
    /// resolved (x, y).
    pub fn text(&mut self, x: Length, y: Length, content: &str) -> TextBuilder<'_> {
        TextBuilder {
            canvas: self,
            x,
            y,
            content: content.to_string(),
            color: Color::TRANSPARENT,
            weight: 400,
            opts: crate::font_shape::ShapeOptions::full(),
            font_kind: None,
            ui_size_q: None,
        }
    }

    /// Number of primitives queued.  Test hook.
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }

    /// Borrow the resolved primitive queue.  Used by the
    /// renderer's flush path.
    pub fn primitives(&self) -> &[Primitive] {
        &self.primitives
    }

    /// Take ownership of the primitive queue, leaving the
    /// Canvas empty.  Used to splice into a larger renderer
    /// queue (e.g. compose nested Canvases).
    pub fn take_primitives(&mut self) -> Vec<Primitive> {
        std::mem::take(&mut self.primitives)
    }

    /// Push an already-resolved primitive.  Internal; builders
    /// call this from `.draw()`.
    fn push(&mut self, p: Primitive) {
        self.primitives.push(p);
    }
}

/// Drawn primitive in physical-pixel coordinates.  Ordered by
/// position in the canvas's queue (submission order = z).
#[derive(Clone, Debug)]
pub enum Primitive {
    Rect(RectPrim),
    Line(LinePrim),
    Text(TextPrim),
}

#[derive(Clone, Debug)]
pub struct RectPrim {
    /// Top-left in physical pixels (window coords).
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub fill: Color,
    /// Corner radius in physical pixels.  0 = sharp.
    pub radius: f64,
    pub border: Option<(f64, Color)>,
    /// (blur, (offset_x, offset_y), color)
    pub shadow: Option<(f64, (f64, f64), Color)>,
}

#[derive(Clone, Debug)]
pub struct LinePrim {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub width: f64,
    pub color: Color,
}

/// Per-`TextPrim` font selection.  `None` means "follow the
/// encoder's global `ui_font` flag" — the legacy behaviour where
/// every chrome `text(...)` call ran through the same font path.
/// `Some(Ui)` opts into SF Pro proportional shaping for ONE text
/// run regardless of the global setting, used by the Font v5
/// showcase to demo chrome rendering while the rest of the dev
/// panel still lays itself out against Monaco cell metrics.
/// `Some(Mono)` does the inverse (forces the mono terminal font)
/// — useful for fixed-pitch demos inside an otherwise SF-Pro page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFontKind {
    Mono,
    Ui,
}

#[derive(Clone, Debug)]
pub struct TextPrim {
    /// Top-left in physical pixels.
    pub x: f64,
    pub y: f64,
    pub content: String,
    pub color: Color,
    /// Phase 5 — CSS weight (100..900, step 100).  Default 400 maps
    /// to the base UI font; other values request a variable-font
    /// variant the renderer materialises lazily.  Ignored for
    /// terminal (mono) text — Monaco has no weight axis to vary.
    pub weight: u16,
    /// Phase 8 — per-context OpenType feature toggles.  Default
    /// `ShapeOptions::full()` matches CTLine's defaults (kerning +
    /// ligatures on); chrome `code` blocks pass `ShapeOptions::code()`
    /// to force uniform glyph advance while keeping ligatures.  PTY
    /// text never reaches this field — the mono renderer doesn't
    /// shape.
    pub opts: crate::font_shape::ShapeOptions,
    /// Per-prim override of the encoder's global `ui_font` flag.
    /// `None` (default) inherits, so existing chrome that built its
    /// layout against Monaco cell widths keeps that font without
    /// every `text(...)` call growing a `.mono()` annotation.
    /// `Some(Ui)` switches just this run through SF Pro shape —
    /// used by the Font v5 showcase rows.
    pub font_kind: Option<TextFontKind>,
    /// Phase 10c — quantised SF Pro pt size (`round(pt × 4)`,
    /// matches `GlyphKey::size_q`).  `None` = use the renderer's
    /// default `UI_FONT_POINT` (13pt) — preserves legacy chrome
    /// behaviour.  `Some(s)` lets callers render SF Pro at any pt
    /// size (showcase title at 40pt, body at 24pt, etc.).  Only
    /// honoured when `font_kind = Some(Ui)`.
    pub ui_size_q: Option<u16>,
}

// ─── Builders ──────────────────────────────────────────────

pub struct RectBuilder<'c> {
    canvas: &'c mut Canvas,
    x: Length,
    y: Length,
    w: Length,
    h: Length,
    fill: Color,
    radius: Pt,
    border: Option<(Pt, Color)>,
    shadow: Option<(Pt, (Pt, Pt), Color)>,
}

impl<'c> RectBuilder<'c> {
    pub fn at(mut self, x: Length, y: Length) -> Self {
        self.x = x;
        self.y = y;
        self
    }
    pub fn size(mut self, w: Length, h: Length) -> Self {
        self.w = w;
        self.h = h;
        self
    }
    pub fn fill(mut self, color: Color) -> Self {
        self.fill = color;
        self
    }
    pub fn radius(mut self, r: Pt) -> Self {
        self.radius = r;
        self
    }
    pub fn border(mut self, width: Pt, color: Color) -> Self {
        self.border = Some((width, color));
        self
    }
    pub fn shadow(mut self, blur: Pt, offset: (Pt, Pt), color: Color) -> Self {
        self.shadow = Some((blur, offset, color));
        self
    }
    pub fn draw(self) {
        let RectBuilder {
            canvas, x, y, w, h, fill, radius, border, shadow,
        } = self;
        let parent = canvas.parent;
        let x_p = parent.x + x.resolve_for_axis(parent.w, canvas.scale);
        let y_p = parent.y + y.resolve_for_axis(parent.h, canvas.scale);
        let w_p = w.resolve_for_axis(parent.w, canvas.scale);
        let h_p = h.resolve_for_axis(parent.h, canvas.scale);
        let prim = RectPrim {
            x: x_p,
            y: y_p,
            w: w_p,
            h: h_p,
            fill,
            radius: radius.to_phys(canvas.scale),
            border: border.map(|(w, c)| (w.to_phys(canvas.scale), c)),
            shadow: shadow.map(|(blur, (ox, oy), c)| {
                (blur.to_phys(canvas.scale), (ox.to_phys(canvas.scale), oy.to_phys(canvas.scale)), c)
            }),
        };
        canvas.push(Primitive::Rect(prim));
    }
}

pub struct LineBuilder<'c> {
    canvas: &'c mut Canvas,
    from: (Length, Length),
    to: (Length, Length),
    width: Pt,
    color: Color,
}

impl<'c> LineBuilder<'c> {
    pub fn stroke(mut self, width: Pt, color: Color) -> Self {
        self.width = width;
        self.color = color;
        self
    }
    pub fn draw(self) {
        let LineBuilder { canvas, from, to, width, color } = self;
        let parent = canvas.parent;
        let fx = parent.x + from.0.resolve_for_axis(parent.w, canvas.scale);
        let fy = parent.y + from.1.resolve_for_axis(parent.h, canvas.scale);
        let tx = parent.x + to.0.resolve_for_axis(parent.w, canvas.scale);
        let ty = parent.y + to.1.resolve_for_axis(parent.h, canvas.scale);
        let prim = LinePrim {
            from: (fx, fy),
            to: (tx, ty),
            width: width.to_phys(canvas.scale),
            color,
        };
        canvas.push(Primitive::Line(prim));
    }
}

pub struct TextBuilder<'c> {
    canvas: &'c mut Canvas,
    x: Length,
    y: Length,
    content: String,
    color: Color,
    weight: u16,
    opts: crate::font_shape::ShapeOptions,
    font_kind: Option<TextFontKind>,
    ui_size_q: Option<u16>,
}

impl<'c> TextBuilder<'c> {
    pub fn color(mut self, c: Color) -> Self {
        self.color = c;
        self
    }
    /// Phase 5 — set the CSS weight for this text run (100..900 in
    /// steps of 100).  `400` is regular; `600` semi-bold; `700`
    /// bold.  Off-step values round to the nearest step.  Only takes
    /// effect for the chrome UI font path (PTY ignores).
    pub fn weight(mut self, weight: u16) -> Self {
        self.weight = weight;
        self
    }
    /// Phase 8 — set the OpenType feature toggles for this run.
    /// Pass `ShapeOptions::all_off()` to suppress ligatures (default
    /// CTLine combines `fi`/`fl`/`==>` etc.) so the dev showcase can
    /// demonstrate the on-vs-off difference; `ShapeOptions::code()`
    /// keeps ligatures but turns kerning off for code-block-style
    /// layout.
    pub fn opts(mut self, opts: crate::font_shape::ShapeOptions) -> Self {
        self.opts = opts;
        self
    }
    /// Force this run through the SF Pro proportional shape path
    /// (`TextFontKind::Ui`), overriding the encoder's global default.
    /// Used by the Font v5 showcase rows so the rest of the dev panel
    /// can still render with mono cell metrics.
    pub fn ui(mut self) -> Self {
        self.font_kind = Some(TextFontKind::Ui);
        self
    }
    /// Force this run through the mono terminal font, regardless of
    /// the encoder's global flag.  Symmetric counterpart to `.ui()`.
    pub fn mono(mut self) -> Self {
        self.font_kind = Some(TextFontKind::Mono);
        self
    }
    /// Phase 10c — set the SF Pro pt size for this run.  `size_q`
    /// is the quantised pt × 4 bucket matching `GlyphKey::size_q`.
    /// Only honoured when `.ui()` is also set.  Default `None` =
    /// renderer's `UI_FONT_POINT` (13pt).
    pub fn ui_size_q(mut self, size_q: u16) -> Self {
        self.ui_size_q = Some(size_q);
        self
    }
    /// Token-based sizing — sets `.ui()` (SF Pro path) AND `.ui_size_q`
    /// in one call to the `UiSize`-derived pt.  Caller picks the token
    /// (Mini / Small / Body / Heading / Title / Display) and per-family
    /// pt is computed by `UiSize::sf_pro_pt()`.  This is the entry
    /// point chrome callsites should reach for — no raw pt in the
    /// canvas chain, no `DEV_PANEL_FONT_SCALE` style hacks.
    pub fn ui_size(mut self, size: crate::ui::view::UiSize) -> Self {
        self.font_kind = Some(TextFontKind::Ui);
        self.ui_size_q = Some(
            crate::glyph_atlas::GlyphKey::size_q_for(size.sf_pro_pt())
        );
        self
    }
    pub fn draw(self) {
        let TextBuilder { canvas, x, y, content, color, weight, opts, font_kind, ui_size_q } = self;
        let parent = canvas.parent;
        let x_p = parent.x + x.resolve_for_axis(parent.w, canvas.scale);
        let y_p = parent.y + y.resolve_for_axis(parent.h, canvas.scale);
        canvas.push(Primitive::Text(TextPrim {
            x: x_p,
            y: y_p,
            content,
            color,
            weight,
            opts,
            font_kind,
            ui_size_q,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_canvas(scale: f64) -> Canvas {
        Canvas::new(scale, ParentRect::window(1000.0, 500.0))
    }

    // ─── Resolution semantics ──────────────────────────────

    #[test]
    fn rect_pt_resolves_via_scale_only() {
        let mut c = window_canvas(2.0);
        c.rect()
            .at(Length::Pt(10.0), Length::Pt(20.0))
            .size(Length::Pt(100.0), Length::Pt(50.0))
            .fill(Color::WHITE)
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        assert_eq!((p.x, p.y, p.w, p.h), (20.0, 40.0, 200.0, 100.0));
    }

    #[test]
    fn rect_pct_resolves_against_parent() {
        let mut c = window_canvas(1.0);
        c.parent = ParentRect { x: 100.0, y: 50.0, w: 200.0, h: 100.0 };
        c.rect()
            .at(Length::Pct(0.5), Length::Pct(0.5))
            .size(Length::Pct(0.25), Length::Pct(0.25))
            .fill(Color::WHITE)
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        // x = 100 + 200*0.5 = 200, y = 50 + 100*0.5 = 100,
        // w = 200*0.25 = 50, h = 100*0.25 = 25
        assert_eq!((p.x, p.y, p.w, p.h), (200.0, 100.0, 50.0, 25.0));
    }

    #[test]
    fn rect_mixed_pt_and_pct_resolves_per_axis() {
        let mut c = window_canvas(2.0);
        c.parent = ParentRect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 };
        c.rect()
            .at(Length::Pt(10.0), Length::Pct(0.1))
            .size(Length::Pct(0.5), Length::Pt(3.0))
            .fill(Color::WHITE)
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        // x = 0 + 10*2 = 20, y = 0 + 200*0.1 = 20,
        // w = 400*0.5 = 200, h = 3*2 = 6
        assert_eq!((p.x, p.y, p.w, p.h), (20.0, 20.0, 200.0, 6.0));
    }

    // ─── Submission order ──────────────────────────────────

    #[test]
    fn submission_order_preserved_in_primitive_queue() {
        let mut c = window_canvas(1.0);
        c.rect().fill(Color::rgb(255, 0, 0)).draw();   // z=0 red
        c.text(Length::Pt(0.0), Length::Pt(0.0), "x").color(Color::WHITE).draw(); // z=1
        c.rect().fill(Color::rgb(0, 255, 0)).draw();   // z=2 green
        let prims = c.primitives();
        assert_eq!(prims.len(), 3);
        match &prims[0] { Primitive::Rect(r) => assert_eq!(r.fill, Color::rgb(255, 0, 0)), _ => panic!() }
        match &prims[1] { Primitive::Text(_) => (), _ => panic!() }
        match &prims[2] { Primitive::Rect(r) => assert_eq!(r.fill, Color::rgb(0, 255, 0)), _ => panic!() }
    }

    // ─── Modifiers ─────────────────────────────────────────

    #[test]
    fn rect_radius_resolves_through_scale() {
        let mut c = window_canvas(2.0);
        c.rect().radius(Pt(4.0)).fill(Color::WHITE).draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        assert_eq!(p.radius, 8.0);
    }

    #[test]
    fn rect_border_resolves_through_scale() {
        let mut c = window_canvas(2.0);
        c.rect()
            .border(Pt(1.0), Color::rgba(0, 0, 0, 0.5))
            .fill(Color::WHITE)
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        let (bw, bc) = p.border.expect("border");
        assert_eq!(bw, 2.0);
        assert_eq!(bc, Color::rgba(0, 0, 0, 0.5));
    }

    #[test]
    fn rect_shadow_resolves_through_scale() {
        let mut c = window_canvas(2.0);
        c.rect()
            .shadow(Pt(16.0), (Pt(0.0), Pt(2.0)), Color::rgba(0, 0, 0, 0.45))
            .fill(Color::WHITE)
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Rect(r) => r.clone(),
            _ => panic!(),
        };
        let (blur, (ox, oy), c2) = p.shadow.expect("shadow");
        assert_eq!(blur, 32.0);
        assert_eq!((ox, oy), (0.0, 4.0));
        assert_eq!(c2, Color::rgba(0, 0, 0, 0.45));
    }

    // ─── Line ──────────────────────────────────────────────

    #[test]
    fn line_resolves_endpoints_and_width() {
        let mut c = window_canvas(2.0);
        c.line(
            (Length::Pt(10.0), Length::Pt(20.0)),
            (Length::Pt(110.0), Length::Pt(20.0)),
        )
        .stroke(Pt(1.0), Color::rgba(255, 255, 255, 0.22))
        .draw();
        let p = match &c.primitives()[0] {
            Primitive::Line(l) => l.clone(),
            _ => panic!(),
        };
        assert_eq!(p.from, (20.0, 40.0));
        assert_eq!(p.to,   (220.0, 40.0));
        assert_eq!(p.width, 2.0);
        assert_eq!(p.color, Color::rgba(255, 255, 255, 0.22));
    }

    // ─── Text ──────────────────────────────────────────────

    #[test]
    fn text_resolves_anchor() {
        let mut c = window_canvas(2.0);
        c.text(Length::Pt(8.0), Length::Pt(12.0), "Copy")
            .color(Color::rgba(255, 255, 255, 0.9))
            .draw();
        let p = match &c.primitives()[0] {
            Primitive::Text(t) => t.clone(),
            _ => panic!(),
        };
        assert_eq!((p.x, p.y), (16.0, 24.0));
        assert_eq!(p.content, "Copy");
    }

    // ─── take_primitives ───────────────────────────────────

    #[test]
    fn take_primitives_empties_the_queue() {
        let mut c = window_canvas(1.0);
        c.rect().fill(Color::WHITE).draw();
        c.rect().fill(Color::BLACK).draw();
        assert_eq!(c.len(), 2);
        let taken = c.take_primitives();
        assert_eq!(taken.len(), 2);
        assert_eq!(c.len(), 0);
        assert!(c.is_empty());
    }
}
