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
    if laid.deco.hidden {
        return;
    }
    // 1. Self decoration first(shadow → bg → border).  CSS paint
    //    order: background-color → background-image → border.  We
    //    add shadow before bg so the shadow looks like it casts
    //    from the box.
    paint_decoration(canvas, &laid.rect, &laid.deco, ctx);

    // 2. Self primitive(if this node is an atom).
    paint_atom(canvas, &laid.view, &laid.rect, ctx);

    // 3. Children — submission order = z order.
    for ch in laid.children.iter() {
        paint_into(canvas, ch, ctx);
    }
}

fn paint_decoration(canvas: &mut Canvas, rect: &super::layout::Rect, deco: &Decoration, ctx: LayoutCtx) {
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
    if let Some(c) = deco.bg { b = b.fill(c); }
    else { b = b.fill(Color::rgba(0, 0, 0, 0.0)); }
    if deco.radius > 0.0 {
        b = b.radius(crate::ui::core::Pt(deco.radius / ctx.scale));
    }
    if let Some((w_phys, c)) = deco.border {
        b = b.border(crate::ui::core::Pt(w_phys / ctx.scale), c);
    }
    if let Some(s) = deco.shadow {
        b = b.shadow(
            crate::ui::core::Pt(s.blur / ctx.scale),
            (
                crate::ui::core::Pt(s.offset.0 / ctx.scale),
                crate::ui::core::Pt(s.offset.1 / ctx.scale),
            ),
            s.color,
        );
    }
    b.draw();
}

fn paint_atom(canvas: &mut Canvas, view: &View, rect: &super::layout::Rect, ctx: LayoutCtx) {
    let phys_to_pt = |phys: f64| Length::Pt(phys / ctx.scale);
    match view {
        View::Text(t) => {
            // Baseline y = top + ascent (NOT top + line_h).
            let baseline_pt = phys_to_pt(rect.y + ctx.ascent_phys);
            // Truncate to fit rect width if Single-line.
            let max_chars = (rect.w / ctx.cell_w_phys).floor().max(0.0) as usize;
            let drawn: String = match &t.lines {
                super::view::TextLines::Single { truncate } => {
                    if t.content.chars().count() <= max_chars {
                        t.content.clone()
                    } else {
                        truncate_text(&t.content, max_chars, *truncate)
                    }
                }
                super::view::TextLines::Wrap { .. } => {
                    // v1: wrap not implemented, treat as single-line.
                    t.content.clone()
                }
            };
            // Color dim if weight=Dim.
            let color = match t.weight {
                super::view::TextWeight::Dim => {
                    let mut c = t.color;
                    c.a *= 0.6;
                    c
                }
                _ => t.color,
            };
            // Horizontal align — compute x_offset from rect.x.
            let content_w_phys = drawn.chars().count() as f64 * ctx.cell_w_phys;
            let x_pad = match t.align {
                super::view::TextAlign::Leading  => 0.0,
                super::view::TextAlign::Center   => (rect.w - content_w_phys) * 0.5,
                super::view::TextAlign::Trailing => rect.w - content_w_phys,
            };
            canvas.text(phys_to_pt(rect.x + x_pad), baseline_pt, &drawn)
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
                .fill(*color)
                .radius(r_pt)
                .draw();
        }
        View::Hairline { color, vertical } => {
            let cx = phys_to_pt(rect.x + if *vertical { rect.w * 0.5 } else { 0.0 });
            let cy = phys_to_pt(rect.y + if *vertical { 0.0 } else { rect.h * 0.5 });
            let end_x = phys_to_pt(rect.x + if *vertical { rect.w * 0.5 } else { rect.w });
            let end_y = phys_to_pt(rect.y + if *vertical { rect.h } else { rect.h * 0.5 });
            canvas.line((cx, cy), (end_x, end_y))
                .stroke(crate::ui::core::Pt(1.0), *color)
                .draw();
        }
        // Stacks / Modified / Spacer don't paint anything on their
        // own — decoration is already painted above, children come
        // next in the recursion.
        _ => {}
    }
}

fn truncate_text(s: &str, max_chars: usize, mode: super::view::Truncate) -> String {
    if max_chars == 0 { return String::new(); }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars { return s.to_string(); }
    match mode {
        super::view::Truncate::End => {
            if max_chars == 0 { String::new() }
            else if max_chars == 1 { "…".into() }
            else {
                let head: String = chars.iter().take(max_chars - 1).collect();
                format!("{head}…")
            }
        }
        super::view::Truncate::Middle => {
            if max_chars < 3 { "…".repeat(max_chars) }
            else {
                let head_len = (max_chars - 1) / 2;
                let tail_len = max_chars - 1 - head_len;
                let head: String = chars.iter().take(head_len).collect();
                let tail: String = chars.iter().skip(chars.len() - tail_len).collect();
                format!("{head}…{tail}")
            }
        }
        super::view::Truncate::None => chars.iter().take(max_chars).collect(),
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
