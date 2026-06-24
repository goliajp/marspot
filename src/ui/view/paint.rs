//! Paint pass — `LaidOut` tree → Canvas primitives.
//!
//! Walks the layed-out tree in submission order(child before
//! parent if you want children on top; for our model we paint
//! parent decoration first, then children, so children paint
//! on top — matches `display: block` parent-then-children CSS).
//!
//! See `docs/ui-system-model.md` §13.

use crate::ui::core::{Canvas, Color, Length};
use super::layout::{Decoration, LaidOut, LayoutCtx};
use super::view::View;

/// Build a Canvas covering the laid-out subtree.  Caller hands
/// the result to `MetalRenderer::encode_canvas_into` or similar.
pub fn paint<'a>(laid: &LaidOut, ctx: LayoutCtx<'a>, parent_w: f64, parent_h: f64) -> Canvas {
    let mut canvas = Canvas::new(ctx.scale, crate::ui::core::ParentRect::window(parent_w, parent_h));
    paint_into(&mut canvas, laid, ctx);
    canvas
}

/// Paint into an existing canvas — used when a higher-level
/// driver(eg dev panel renderer)wants ONE canvas for the whole
/// frame instead of one per subtree.
pub fn paint_into<'a>(canvas: &mut Canvas, laid: &LaidOut, ctx: LayoutCtx<'a>) {
    paint_into_inner(canvas, laid, ctx, None, 1.0);
}

/// Paint with optional viewport clip + cumulative opacity.  `clip =
/// Some` = ScrollView or `.clip()` modifier in effect; descendants
/// whose rect lies fully outside the clip are skipped(viewport
/// culling).  `opacity_mult` multiplies down through subtrees so
/// `.opacity(0.5)` on a parent darkens every descendant proportional.
fn paint_into_inner<'a>(
    canvas: &mut Canvas,
    laid: &LaidOut,
    ctx: LayoutCtx<'a>,
    clip: Option<&super::layout::Rect>,
    opacity_mult: f64,
) {
    if laid.deco.hidden {
        return;
    }
    // Cull if rect is entirely outside the active clip.
    if let Some(c) = clip {
        let rect = &laid.rect;
        let outside =
            rect.x + rect.w < c.x ||
            rect.x > c.x + c.w ||
            rect.y + rect.h < c.y ||
            rect.y > c.y + c.h;
        if outside { return; }
    }
    // Multiply in this node's own opacity for self + descendants.
    let local_opacity = (opacity_mult * laid.deco.opacity).clamp(0.0, 1.0);
    if local_opacity <= 0.0 { return; }

    // 1. Self decoration first(shadow → bg → border).
    paint_decoration(canvas, &laid.rect, &laid.deco, ctx, local_opacity);

    // 2. Self primitive(if this node is an atom).
    paint_atom(canvas, &laid.view, &laid.rect, ctx, local_opacity);

    // 3. Children — submission order = z order.  ScrollView OR a
    //    `.clip()` modifier establishes a clip for descendants.
    let establishes_clip =
        matches!(&laid.view, View::ScrollView { .. }) || laid.deco.clip.is_some();
    let child_clip = if establishes_clip { Some(&laid.rect) } else { clip };
    for ch in laid.children.iter() {
        paint_into_inner(canvas, ch, ctx, child_clip, local_opacity);
    }
}

fn mul_alpha(c: Color, m: f64) -> Color {
    Color { a: (c.a * m).clamp(0.0, 1.0), ..c }
}

/// Approximate a linear gradient as N solid bands.  Cheap, OK-looking
/// for chrome polish use cases.  Real gradient primitive needs Metal
/// pipeline support — v3 follow-up.
fn paint_gradient_bands<'a>(canvas: &mut Canvas, rect: &super::layout::Rect, g: &super::view::LinearGradient, ctx: LayoutCtx<'a>, opacity: f64) {
    if g.stops.is_empty() { return; }
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    const BANDS: usize = 16;
    for i in 0..BANDS {
        let t = i as f64 / BANDS as f64;
        let color = sample_gradient(&g.stops, t);
        let (bx, by, bw, bh) = match g.direction {
            super::view::GradientDir::TopToBottom => {
                let h = rect.h / BANDS as f64;
                (rect.x, rect.y + t * rect.h, rect.w, h)
            }
            super::view::GradientDir::BottomToTop => {
                let h = rect.h / BANDS as f64;
                (rect.x, rect.y + (1.0 - t - 1.0 / BANDS as f64) * rect.h, rect.w, h)
            }
            super::view::GradientDir::LeftToRight => {
                let w = rect.w / BANDS as f64;
                (rect.x + t * rect.w, rect.y, w, rect.h)
            }
            super::view::GradientDir::RightToLeft => {
                let w = rect.w / BANDS as f64;
                (rect.x + (1.0 - t - 1.0 / BANDS as f64) * rect.w, rect.y, w, rect.h)
            }
        };
        canvas.rect()
            .at(phys_to_pt(bx), phys_to_pt(by))
            .size(phys_to_pt(bw), phys_to_pt(bh))
            .fill(mul_alpha(color, opacity))
            .draw();
    }
}

fn sample_gradient(stops: &[(f64, Color)], t: f64) -> Color {
    if stops.is_empty() { return Color::rgba(0, 0, 0, 0.0); }
    if t <= stops[0].0 { return stops[0].1; }
    if t >= stops.last().unwrap().0 { return stops.last().unwrap().1; }
    for w in stops.windows(2) {
        let (t0, c0) = (w[0].0, w[0].1);
        let (t1, c1) = (w[1].0, w[1].1);
        if t >= t0 && t <= t1 {
            let f = (t - t0) / (t1 - t0).max(1e-6);
            return Color {
                r: ((c0.r as f64) * (1.0 - f) + (c1.r as f64) * f) as u8,
                g: ((c0.g as f64) * (1.0 - f) + (c1.g as f64) * f) as u8,
                b: ((c0.b as f64) * (1.0 - f) + (c1.b as f64) * f) as u8,
                a: c0.a * (1.0 - f) + c1.a * f,
            };
        }
    }
    stops[0].1
}

fn paint_decoration<'a>(canvas: &mut Canvas, rect: &super::layout::Rect, deco: &Decoration, ctx: LayoutCtx<'a>, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    let r_x = phys_to_pt(rect.x);
    let r_y = phys_to_pt(rect.y);
    let r_w = phys_to_pt(rect.w);
    let r_h = phys_to_pt(rect.h);

    if let Some(g) = &deco.bg_gradient {
        // Paint as N solid bands along the gradient axis.  Each band
        // gets the interpolated color from the stops.  N = 16 by
        // default — enough to look smooth in most cases.
        paint_gradient_bands(canvas, rect, g, ctx, opacity);
    }
    if deco.bg.is_none() && deco.border.is_none() && deco.shadow.is_none() {
        return;
    }
    let mut b = canvas.rect()
        .at(r_x, r_y)
        .size(r_w, r_h);
    if let Some(c) = deco.bg { b = b.fill(mul_alpha(c, opacity)); }
    else { b = b.fill(Color::rgba(0, 0, 0, 0.0)); }
    if deco.radius > 0.0 {
        b = b.radius(crate::ui::core::Pt(deco.radius / ctx.scale));
    }
    if let Some((w_phys, c)) = deco.border {
        b = b.border(crate::ui::core::Pt(w_phys / ctx.scale), mul_alpha(c, opacity));
    }
    if let Some(s) = deco.shadow {
        b = b.shadow(
            crate::ui::core::Pt(s.blur / ctx.scale),
            (
                crate::ui::core::Pt(s.offset.0 / ctx.scale),
                crate::ui::core::Pt(s.offset.1 / ctx.scale),
            ),
            mul_alpha(s.color, opacity),
        );
    }
    b.draw();
}

fn paint_atom<'a>(canvas: &mut Canvas, view: &View, rect: &super::layout::Rect, ctx: LayoutCtx<'a>, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    match view {
        View::Text(t) => {
            // `TextPrim` is documented top-left in physical pixels;
            // the renderer converts `y → y + ascent` internally to
            // hit the baseline.  So we pass `rect.y` as-is here —
            // emitting `rect.y + ascent` would shift everything down
            // by an ascent (~12 pt), driving subsequent rows into
            // each other (real bug, observed 2026-06-23 dev panel
            // overlap on L1 / L4).
            let top_pt = phys_to_pt(rect.y);
            // Phase 10c — measure for truncation + alignment via the
            // SAME provider the layout pass used, so widths agree
            // regardless of which font this text runs through.
            let measured_w_phys = ctx.fonts.advance_phys(&t.content, t.font);
            let drawn: String = match &t.lines {
                super::view::TextLines::Single { truncate } => {
                    if measured_w_phys <= rect.w {
                        t.content.clone()
                    } else {
                        // Truncate by cells — good enough for mono;
                        // for Ui (proportional) it's an approximation
                        // (avg cell pitch) that won't catch every
                        // case but stays simpler than per-glyph
                        // binary search.  Wrap will get the careful
                        // path in v2.
                        let max_cells = (rect.w / ctx.cell_w_phys).floor().max(0.0) as usize;
                        truncate_text(&t.content, max_cells, *truncate)
                    }
                }
                super::view::TextLines::Wrap { .. } => t.content.clone(),
            };
            let color = mul_alpha(t.color, opacity);
            // Re-measure the (potentially truncated) string for align.
            let drawn_w_phys = ctx.fonts.advance_phys(&drawn, t.font);
            let x_pad = match t.align {
                super::view::TextAlign::Leading  => 0.0,
                super::view::TextAlign::Center   => (rect.w - drawn_w_phys) * 0.5,
                super::view::TextAlign::Trailing => rect.w - drawn_w_phys,
            };
            // Phase 10c — map view's `TextFontSpec` onto the canvas
            // TextPrim's per-run override fields so the renderer's
            // chrome path knows whether to take the SF Pro shape +
            // colour-emoji route or stay on Monaco mono.  Mono runs
            // emit through the default `text(...)` path (font_kind
            // = None → inherit encoder's global `ui_font`).
            let mut builder = canvas.text(phys_to_pt(rect.x + x_pad), top_pt, &drawn)
                .color(color);
            if let super::view::TextFontSpec::Ui { size_q, weight, opts_bits } = t.font {
                builder = builder
                    .ui()
                    .ui_size_q(size_q)
                    .weight(weight)
                    .opts(super::view::unpack_shape_opts(opts_bits));
            }
            builder.draw();
        }
        View::Filled { color, radius } => {
            let r_pt = match radius {
                Length::Pt(p) => crate::ui::core::Pt(*p),
                _ => crate::ui::core::Pt(0.0),
            };
            canvas.rect()
                .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
                .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
                .fill(mul_alpha(*color, opacity))
                .radius(r_pt)
                .draw();
        }
        View::Hairline { color, vertical } => {
            let cx = phys_to_pt(rect.x + if *vertical { rect.w * 0.5 } else { 0.0 });
            let cy = phys_to_pt(rect.y + if *vertical { 0.0 } else { rect.h * 0.5 });
            let end_x = phys_to_pt(rect.x + if *vertical { rect.w * 0.5 } else { rect.w });
            let end_y = phys_to_pt(rect.y + if *vertical { rect.h } else { rect.h * 0.5 });
            canvas.line((cx, cy), (end_x, end_y))
                .stroke(crate::ui::core::Pt(1.0), mul_alpha(*color, opacity))
                .draw();
        }
        View::Toggle { id } => {
            paint_toggle(canvas, rect, *id, ctx, opacity);
        }
        View::Picker { id, options } => {
            paint_picker(canvas, rect, *id, options, ctx, opacity);
        }
        View::Image(img) => {
            // v1 stub: paint a tinted placeholder rect.  Real Image
            // primitive support lands when renderer adds it.
            let placeholder_color = img.tint
                .unwrap_or(crate::ui::theme::color::BG_HOVER);
            canvas.rect()
                .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
                .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
                .fill(mul_alpha(placeholder_color, opacity))
                .draw();
        }
        View::Shape(spec) => {
            paint_shape(canvas, spec, rect, ctx, opacity);
        }
        // Stacks / Modified / Spacer / LazyVStack / ScrollView don't
        // paint anything on their own — decoration is already painted
        // above, children come next in the recursion.
        _ => {}
    }
}

/// Toggle = capsule with a circle inside.  Visual state derived
/// from `HostState[id]::ToggleState` via `with_host_state`.  If no
/// state, defaults to off.
fn paint_toggle<'a>(canvas: &mut Canvas, rect: &super::layout::Rect, id: super::types::ViewId, ctx: LayoutCtx<'a>, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    let on = super::state::with_host_state(|s| {
        s.get::<super::ToggleState>(id).map(|t| t.on).unwrap_or(false)
    });
    let track_color = if on {
        crate::ui::theme::color::ACCENT
    } else {
        crate::ui::theme::color::BG_HOVER
    };
    let knob_color = crate::ui::theme::color::FG;
    let r = rect.h * 0.5;
    canvas.rect()
        .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
        .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
        .fill(mul_alpha(track_color, opacity))
        .radius(crate::ui::core::Pt(r / ctx.scale))
        .draw();
    let knob_d = rect.h * 0.8;
    let knob_inset = (rect.h - knob_d) * 0.5;
    let knob_x = if on { rect.x + rect.w - knob_d - knob_inset } else { rect.x + knob_inset };
    let knob_y = rect.y + knob_inset;
    canvas.rect()
        .at(phys_to_pt(knob_x), phys_to_pt(knob_y))
        .size(phys_to_pt(knob_d), phys_to_pt(knob_d))
        .fill(mul_alpha(knob_color, opacity))
        .radius(crate::ui::core::Pt(knob_d * 0.5 / ctx.scale))
        .draw();
}

/// Picker = horizontal segmented control.  Highlights the slot
/// currently in `HostState[id]::PickerState`.
fn paint_picker<'a>(canvas: &mut Canvas, rect: &super::layout::Rect, id: super::types::ViewId, options: &[String], ctx: LayoutCtx<'a>, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    if options.is_empty() { return; }
    let n = options.len() as f64;
    let seg_w = rect.w / n;
    let selected = super::state::with_host_state(|s| {
        s.get::<super::PickerState>(id).map(|p| p.selected).unwrap_or(0)
    });
    // Track BG.
    canvas.rect()
        .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
        .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
        .fill(mul_alpha(crate::ui::theme::color::BG_PANEL, opacity))
        .radius(crate::ui::core::Pt(4.0))
        .draw();
    // Selected segment highlight.
    if selected < options.len() {
        canvas.rect()
            .at(phys_to_pt(rect.x + selected as f64 * seg_w), phys_to_pt(rect.y))
            .size(phys_to_pt(seg_w), phys_to_pt(rect.h))
            .fill(mul_alpha(crate::ui::theme::color::ACCENT, opacity))
            .radius(crate::ui::core::Pt(4.0))
            .draw();
    }
    // Labels — naive center placement.  Real text alignment lands
    // when picker has its own layout pass; v1 quick-and-clean.
    for (i, label) in options.iter().enumerate() {
        let w = super::layout::text_width_cells(label) as f64 * ctx.cell_w_phys;
        let seg_left = rect.x + i as f64 * seg_w;
        let tx = seg_left + (seg_w - w) * 0.5;
        let ty = rect.y + (rect.h - ctx.cell_h_phys) * 0.5;
        let color = if i == selected {
            crate::ui::theme::color::BG
        } else {
            crate::ui::theme::color::FG
        };
        canvas.text(phys_to_pt(tx), phys_to_pt(ty), label)
            .color(mul_alpha(color, opacity))
            .draw();
    }
}

fn paint_shape<'a>(canvas: &mut Canvas, spec: &super::view::ShapeSpec, rect: &super::layout::Rect, ctx: LayoutCtx<'a>, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    use super::view::ShapeSpec::*;
    match spec {
        Circle { fill } => {
            // Approximate via rounded-rect with radius = half the
            // smaller dim.  Real circle/SDF lands when renderer adds.
            let r = rect.w.min(rect.h) * 0.5;
            canvas.rect()
                .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
                .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
                .fill(mul_alpha(*fill, opacity))
                .radius(crate::ui::core::Pt(r / ctx.scale))
                .draw();
        }
        Capsule { fill } => {
            let r = rect.h * 0.5;
            canvas.rect()
                .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
                .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
                .fill(mul_alpha(*fill, opacity))
                .radius(crate::ui::core::Pt(r / ctx.scale))
                .draw();
        }
        RoundedRect { radius, fill } => {
            let r_pt = match radius {
                Length::Pt(p) => crate::ui::core::Pt(*p),
                _ => crate::ui::core::Pt(4.0),
            };
            canvas.rect()
                .at(phys_to_pt(rect.x), phys_to_pt(rect.y))
                .size(phys_to_pt(rect.w), phys_to_pt(rect.h))
                .fill(mul_alpha(*fill, opacity))
                .radius(r_pt)
                .draw();
        }
    }
}

/// Truncate `s` to fit within `max_cells` display columns (CJK = 2).
/// Greedy: walk chars, sum widths, stop when adding the next char
/// would exceed budget.  Reserve 1 cell for the ellipsis when
/// truncate mode adds one.
fn truncate_text(s: &str, max_cells: usize, mode: super::view::Truncate) -> String {
    if max_cells == 0 { return String::new(); }
    if super::layout::text_width_cells(s) <= max_cells {
        return s.to_string();
    }
    let take_to_cells = |budget: usize| -> String {
        let mut acc = String::new();
        let mut used = 0usize;
        for c in s.chars() {
            let w = marspot_term::grid::char_width(c) as usize;
            if used + w > budget { break; }
            acc.push(c);
            used += w;
        }
        acc
    };
    match mode {
        super::view::Truncate::End => {
            if max_cells == 1 { "…".into() }
            else {
                let head = take_to_cells(max_cells - 1);
                format!("{head}…")
            }
        }
        super::view::Truncate::Middle => {
            if max_cells < 3 { "…".repeat(max_cells) }
            else {
                let half = (max_cells - 1) / 2;
                let head = take_to_cells(half);
                // For tail, walk from the back.
                let mut tail_chars: Vec<char> = Vec::new();
                let mut used = 0usize;
                for c in s.chars().rev() {
                    let w = marspot_term::grid::char_width(c) as usize;
                    if used + w > (max_cells - 1 - half) { break; }
                    tail_chars.push(c);
                    used += w;
                }
                let tail: String = tail_chars.iter().rev().collect();
                format!("{head}…{tail}")
            }
        }
        super::view::Truncate::None => take_to_cells(max_cells),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::view::Truncate;

    #[test]
    fn truncate_end_appends_ellipsis() {
        assert_eq!(truncate_text("hello world", 7, Truncate::End), "hello …");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let r = truncate_text("hello world", 7, Truncate::Middle);
        assert_eq!(r.chars().count(), 7);
        assert!(r.contains('…'));
    }

    #[test]
    fn truncate_none_is_hard_cut() {
        assert_eq!(truncate_text("hello world", 5, Truncate::None), "hello");
    }
}
