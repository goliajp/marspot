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
pub fn paint(laid: &LaidOut, ctx: LayoutCtx, parent_w: f64, parent_h: f64) -> Canvas {
    let mut canvas = Canvas::new(ctx.scale, crate::ui::core::ParentRect::window(parent_w, parent_h));
    paint_into(&mut canvas, laid, ctx);
    canvas
}

/// Paint into an existing canvas — used when a higher-level
/// driver(eg dev panel renderer)wants ONE canvas for the whole
/// frame instead of one per subtree.
pub fn paint_into(canvas: &mut Canvas, laid: &LaidOut, ctx: LayoutCtx) {
    paint_into_inner(canvas, laid, ctx, None, 1.0);
}

/// Paint with optional viewport clip + cumulative opacity.  `clip =
/// Some` = ScrollView or `.clip()` modifier in effect; descendants
/// whose rect lies fully outside the clip are skipped(viewport
/// culling).  `opacity_mult` multiplies down through subtrees so
/// `.opacity(0.5)` on a parent darkens every descendant proportional.
fn paint_into_inner(
    canvas: &mut Canvas,
    laid: &LaidOut,
    ctx: LayoutCtx,
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

fn paint_decoration(canvas: &mut Canvas, rect: &super::layout::Rect, deco: &Decoration, ctx: LayoutCtx, opacity: f64) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    let r_x = phys_to_pt(rect.x);
    let r_y = phys_to_pt(rect.y);
    let r_w = phys_to_pt(rect.w);
    let r_h = phys_to_pt(rect.h);

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

fn paint_atom(canvas: &mut Canvas, view: &View, rect: &super::layout::Rect, ctx: LayoutCtx, opacity: f64) {
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
            // Truncate to fit rect width if Single-line.  Width
            // measured in cells (CJK = 2) via the same shared
            // helper as layout, so widths agree.
            let max_cells = (rect.w / ctx.cell_w_phys).floor().max(0.0) as usize;
            let drawn: String = match &t.lines {
                super::view::TextLines::Single { truncate } => {
                    if super::layout::text_width_cells(&t.content) <= max_cells {
                        t.content.clone()
                    } else {
                        truncate_text(&t.content, max_cells, *truncate)
                    }
                }
                super::view::TextLines::Wrap { .. } => {
                    // v1: wrap not implemented, treat as single-line.
                    t.content.clone()
                }
            };
            // Weight is currently a no-op in the renderer (chrome font
            // has no bold cut).  v2+ will swap glyph variants per
            // weight when SDF supports it.  Color is the source of
            // visual differentiation — callers use tokens like
            // `theme::text::HINT` / `CAPTION` to vary perceived weight.
            // Multiply alpha by opacity for Opacity-modified subtrees.
            let color = mul_alpha(t.color, opacity);
            // Horizontal align — compute x_offset from rect.x.
            let content_w_phys = super::layout::text_width_cells(&drawn) as f64 * ctx.cell_w_phys;
            let x_pad = match t.align {
                super::view::TextAlign::Leading  => 0.0,
                super::view::TextAlign::Center   => (rect.w - content_w_phys) * 0.5,
                super::view::TextAlign::Trailing => rect.w - content_w_phys,
            };
            canvas.text(phys_to_pt(rect.x + x_pad), top_pt, &drawn)
                .color(color)
                .draw();
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
        // Stacks / Modified / Spacer don't paint anything on their
        // own — decoration is already painted above, children come
        // next in the recursion.
        _ => {}
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
