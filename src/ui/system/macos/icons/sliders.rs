//! Settings icon — two horizontal sliders with their knobs at
//! different positions.
//!
//! Stroked geometry like every other toolbar icon (no glyph, no
//! font dependency), and deliberately *not* a gear: the neighbours
//! are all rectilinear line-work, and a gear's radial silhouette
//! reads as a different family at 16 px.  Two rails with offset
//! knobs also says what the panel is — values you move — rather
//! than the generic "configuration".

use marspot_term::layout::Rect;
use crate::ui::core::{IconComponent, ViewPainter};
use super::{icon_stroke, icon_inner_rect};
use super::grid::ui_fill;

pub struct SlidersIcon;

impl IconComponent for SlidersIcon {
    fn paint(&self, p: &mut ViewPainter, rect: Rect, fg: [f32; 4]) {
        let inner = icon_inner_rect(rect);
        let stroke = icon_stroke(inner);
        // Two rails at 1/3 and 2/3 height, knobs at 2/3 and 1/3
        // across — offset from each other so the icon reads as
        // "adjustable" rather than as an equals sign.
        let knob_w = (stroke * 2.0).min(inner.w * 0.22).max(stroke);
        let knob_h = (stroke * 3.0).min(inner.h * 0.5).max(stroke);
        for (row, knob_at) in [(1.0 / 3.0, 0.62), (2.0 / 3.0, 0.28)] {
            let rail_y = inner.y_top + inner.h * row - stroke * 0.5;
            ui_fill(
                p,
                Rect { x: inner.x, y_top: rail_y, w: inner.w, h: stroke },
                fg,
            );
            // Knob centred on the rail, clamped so it never hangs off
            // either end however small the icon gets.
            let kx = (inner.x + inner.w * knob_at - knob_w * 0.5)
                .clamp(inner.x, inner.x + inner.w - knob_w);
            ui_fill(
                p,
                Rect {
                    x: kx,
                    y_top: rail_y + stroke * 0.5 - knob_h * 0.5,
                    w: knob_w,
                    h: knob_h,
                },
                fg,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The knobs must stay inside the icon box at any size — an icon
    /// that paints outside its rect bleeds into its neighbour, and
    /// the toolbar packs these 6 px apart.
    #[test]
    fn the_knobs_stay_inside_the_icon_box() {
        for size in [10.0f64, 16.0, 24.0, 64.0] {
            let rect = Rect { x: 100.0, y_top: 50.0, w: size, h: size };
            let inner = icon_inner_rect(rect);
            let stroke = icon_stroke(inner);
            let knob_w = (stroke * 2.0).min(inner.w * 0.22).max(stroke);
            for knob_at in [0.62f64, 0.28] {
                let kx = (inner.x + inner.w * knob_at - knob_w * 0.5)
                    .clamp(inner.x, inner.x + inner.w - knob_w);
                assert!(kx >= inner.x, "size {size}: knob left of the box");
                assert!(
                    kx + knob_w <= inner.x + inner.w + 1e-9,
                    "size {size}: knob right of the box"
                );
            }
        }
    }
}
