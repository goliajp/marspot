//! A `Canvas`'s primitives turned into instances, grouped into runs
//! a backend can draw in submission order.

use crate::font_cache::FontCache;
use crate::frame_build::color::{rgba8_of, rgba8_of_f32};
use crate::frame_build::text_run::{push_text_run_kind, push_text_run_ui_shaped, FontKind};
use crate::glyph_atlas::GlyphAtlas;
use golia_ui_core::scene::{GlyphInstance, UiRectInstance};

/// Convert a `RectPrim` to a `UiRectInstance` for the ui_rects
/// pipeline.  Border / shadow optional fields fold cleanly into
/// the shader's existing knobs (0 / TRANSPARENT = skip).
fn ui_rect_instance_from_rect(r: &crate::ui::core::canvas::RectPrim) -> UiRectInstance {
    let (border_w, border_c) = r.border
        .map(|(w, c)| (w as f32, c.to_rgba_f32()))
        .unwrap_or((0.0, [0.0; 4]));
    // Intensity once, in the colour's alpha. This used to pass `c.a`
    // here as well, and the fragment multiplies the two -- so a rect
    // asking for a 45% shadow was drawn at 20%. The shader now holds
    // the second knob at 1 for the published format; this is the same
    // rule on the older path.
    let (shadow_blur, shadow_alpha, shadow_color) = r.shadow
        .map(|(blur, _offset, c)| (blur as f32, 1.0f32, c.to_rgba_f32()))
        .unwrap_or((0.0, 0.0, [0.0; 4]));
    let _ = shadow_alpha;
    UiRectInstance {
        origin: [r.x as f32, r.y as f32],
        size:   [r.w as f32, r.h as f32],
        fill: rgba8_of(r.fill),
        border: rgba8_of_f32(border_c),
        radius: r.radius as f32,
        border_width: border_w,
        shadow_offset: [0.0, 0.0],
        shadow_color: rgba8_of_f32(shadow_color),
        shadow_blur,
    }
}

/// Axis-aligned horizontal or vertical line, emitted as a
/// degenerate `UiRectInstance` with radius=0.  Width = stroke
/// width.  Non-axis-aligned lines aren't supported yet — they'd
/// need a rotated line shader.
fn ui_rect_instance_from_line(l: &crate::ui::core::canvas::LinePrim) -> UiRectInstance {
    let (x, y, w, h) = if (l.from.1 - l.to.1).abs() < 0.5 {
        // Horizontal line.
        let x_min = l.from.0.min(l.to.0);
        let x_max = l.from.0.max(l.to.0);
        let cy = (l.from.1 + l.to.1) * 0.5;
        (x_min, cy - l.width * 0.5, x_max - x_min, l.width)
    } else if (l.from.0 - l.to.0).abs() < 0.5 {
        // Vertical line.
        let y_min = l.from.1.min(l.to.1);
        let y_max = l.from.1.max(l.to.1);
        let cx = (l.from.0 + l.to.0) * 0.5;
        (cx - l.width * 0.5, y_min, l.width, y_max - y_min)
    } else {
        // Diagonal — degenerate fallback: bounding box.  Caller
        // hits this only on accidental misuse; lines should be
        // axis-aligned for now.
        let x = l.from.0.min(l.to.0);
        let y = l.from.1.min(l.to.1);
        let w = (l.from.0 - l.to.0).abs().max(l.width);
        let h = (l.from.1 - l.to.1).abs().max(l.width);
        (x, y, w, h)
    };
    UiRectInstance {
        origin: [x as f32, y as f32],
        size:   [w as f32, h as f32],
        fill: rgba8_of(l.color),
        border: golia_ui_core::Rgba8::TRANSPARENT,
        radius: 0.0,
        border_width: 0.0,
        shadow_offset: [0.0, 0.0],
        shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
        shadow_blur: 0.0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CanvasRunKind {
    UiRect,
    Glyph,
    /// Phase 7 — colour-emoji glyph run.  Drawn through the
    /// `fg_color_pipeline` sampling the BGRA `color_atlas` so the
    /// glyph emits real colours rather than alpha-only silhouette.
    ColorGlyph,
}

pub(crate) struct CanvasRun {
    pub(crate) kind: CanvasRunKind,
    pub(crate) count: usize,
}

/// Walk a Canvas's primitives in submission order, emit instances
/// into the two flat buffers, and record contiguous runs by
/// pipeline kind so the encoder can switch pipelines at run
/// boundaries (preserving submission-order = z-order).
///
/// `font_metrics` carries the cell-grid sizing the chrome font
/// uses; the renderer's existing `push_text_run` consumes them
/// the same way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_canvas_runs(
    canvas: &crate::ui::core::canvas::Canvas,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w_f: f32,
    atlas_h_f: f32,
    color_atlas_w_f: f32,
    color_atlas_h_f: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    out_ui: &mut Vec<UiRectInstance>,
    out_glyphs: &mut Vec<GlyphInstance>,
    out_color_glyphs: &mut Vec<GlyphInstance>,
    ui_font: bool,
) -> Vec<CanvasRun> {
    use crate::ui::core::canvas::Primitive;

    let mut runs: Vec<CanvasRun> = Vec::new();
    let mut cur: Option<CanvasRunKind> = None;
    let bump = |runs: &mut Vec<CanvasRun>, k: CanvasRunKind, n: usize| {
        if runs.last().map(|r| r.kind == k).unwrap_or(false) {
            runs.last_mut().unwrap().count += n;
        } else {
            runs.push(CanvasRun { kind: k, count: n });
        }
    };

    for p in canvas.primitives() {
        match p {
            Primitive::Rect(r) => {
                out_ui.push(ui_rect_instance_from_rect(r));
                bump(&mut runs, CanvasRunKind::UiRect, 1);
                cur = Some(CanvasRunKind::UiRect);
            }
            Primitive::Line(l) => {
                out_ui.push(ui_rect_instance_from_line(l));
                bump(&mut runs, CanvasRunKind::UiRect, 1);
                cur = Some(CanvasRunKind::UiRect);
            }
            Primitive::Text(t) => {
                let mono_before = out_glyphs.len();
                let color_before = out_color_glyphs.len();
                let baseline_y = t.y as f32 + ascent;
                // Per-prim font_kind overrides the encoder's global
                // `ui_font`.  `None` (the default for every `text(...)`
                // call) inherits, so chrome that laid itself out
                // against mono cell metrics keeps that font without
                // having to grow `.mono()` annotations everywhere.
                let use_ui = match t.font_kind {
                    Some(crate::ui::core::canvas::TextFontKind::Ui) => true,
                    Some(crate::ui::core::canvas::TextFontKind::Mono) => false,
                    None => ui_font,
                };
                if use_ui {
                    // SF Pro path: pass raw y_top_phys (not baseline_y).
                    // push_text_run_ui_shaped derives baseline from the
                    // run's own ascent — correct geometry across UiSize.
                    push_text_run_ui_shaped(
                        &t.content,
                        t.x as f32,
                        t.y as f32,
                        t.color.to_rgba_f32(),
                        ascent,
                        atlas_w_f, atlas_h_f,
                        color_atlas_w_f, color_atlas_h_f,
                        t.weight,
                        t.opts,
                        t.ui_size_q,
                        font, atlas, color_atlas, out_glyphs, out_color_glyphs,
                    );
                } else {
                    push_text_run_kind(
                        &t.content,
                        t.x as f32,
                        baseline_y,
                        t.color.to_rgba_f32(),
                        cell_w, cell_h, ascent,
                        atlas_w_f, atlas_h_f,
                        font, atlas, out_glyphs,
                        FontKind::Terminal,
                    );
                }
                // Phase 7 — the shape path can interleave mono +
                // colour glyphs in a single TextPrim (LTR Latin then
                // a fallback 👍 then more Latin).  We collapse to ONE
                // run per kind here: all of this text's mono glyphs
                // go in a `Glyph` run, all of its colour glyphs go in
                // a `ColorGlyph` run.  Submission-order within each
                // sink is preserved, and visually identical pixels
                // because the mono FG pass and the colour FG pass
                // composite to the same target with the same
                // pre-multiplied blend.
                let mono_added = out_glyphs.len() - mono_before;
                if mono_added > 0 {
                    bump(&mut runs, CanvasRunKind::Glyph, mono_added);
                    cur = Some(CanvasRunKind::Glyph);
                }
                let color_added = out_color_glyphs.len() - color_before;
                if color_added > 0 {
                    bump(&mut runs, CanvasRunKind::ColorGlyph, color_added);
                    cur = Some(CanvasRunKind::ColorGlyph);
                }
            }
        }
    }
    let _ = cur;
    runs
}
