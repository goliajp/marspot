//! macOS-style window-chrome traffic lights: red close, yellow
//! minimize, green maximize.  Drawn as small filled discs (SDF
//! corner_radius = size/2) anchored to the left edge of a title bar.

use marspot_term::layout::Rect;
use crate::render_metal::UiRectInstance;

/// Layout: 3 dots anchored left.  Rects use physical px.
#[derive(Debug, Clone)]
pub struct TrafficLights {
    pub close: Rect,
    pub min: Rect,
    pub max: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficLightHit {
    Close,
    Minimize,
    Maximize,
}

/// Traffic-light geometry, in logical points (callers multiply by the
/// backing scale).  These are the single source of truth for every
/// custom-drawn title bar; the main window uses the OS's own native
/// buttons and never touches these.
///
/// 15 rather than the nominal 12 of the macOS spec.  Measured against
/// the OS's own buttons rendered beside ours (pixel-counted from a
/// screenshot, not eyeballed): native shows **14 px** of saturated
/// colour on this display.  A 15 px disc loses about a pixel to SDF
/// antialiasing, landing on the same visible 14.  Gap stays 8.
pub const LIGHT_SIZE_LOGICAL: f64 = 15.0;
pub const LIGHT_GAP_LOGICAL: f64 = 8.0;
pub const LIGHT_LEFT_PAD_LOGICAL: f64 = 12.0;

/// Hover glyphs, as pixel masks on a normalised grid.
///
/// The shapes are diagonals — an X for close, two triangles split by a
/// diagonal seam for zoom — and `ui_rect` only draws axis-aligned
/// rectangles, so they cannot be stroked.  Drawing them as *text* was
/// the first attempt and it does not hold up: the glyph comes out at
/// whatever size the font's cell is rather than the disc's, and its ink
/// box does not centre on the disc (measured against the system's own
/// buttons: our ✕ rendered 6×11 px where the OS draws 6×6).
///
/// A mask gives exact control and no font dependency.  Each entry is
/// `(row, x_start, x_end_exclusive)` on the grid named by `GRID`, and
/// the runs are scaled to the disc at paint time.
pub struct IconMask {
    /// Mask dimensions in grid cells.  Width and height are separate —
    /// the minimise bar is 8×2, so squaring the grid would park it
    /// against the top of the disc instead of its middle.
    pub grid_w: f64,
    pub grid_h: f64,
    /// Mask width as a fraction of the disc's diameter.
    pub extent: f64,
    /// `(row, x_start, x_end_exclusive)` runs.
    pub runs: &'static [(u8, u8, u8)],
}

/// ✕ — two 2-px diagonals crossing.
pub const ICON_CLOSE: IconMask = IconMask {
    grid_w: 6.0,
    grid_h: 6.0,
    extent: 0.43,
    runs: &[
        (0, 0, 2), (0, 4, 6),
        (1, 0, 6),
        (2, 1, 5),
        (3, 1, 5),
        (4, 0, 6),
        (5, 0, 2), (5, 4, 6),
    ],
};

/// − — a single bar, wider and thinner than the close mark.
pub const ICON_MIN: IconMask = IconMask {
    grid_w: 8.0,
    grid_h: 2.0,
    extent: 0.57,
    runs: &[(0, 0, 8), (1, 0, 8)],
};

/// Zoom — two triangles split by a diagonal seam.
pub const ICON_ZOOM: IconMask = IconMask {
    grid_w: 7.0,
    grid_h: 6.0,
    extent: 0.50,
    runs: &[
        (0, 0, 5),
        (1, 0, 4), (1, 5, 6),
        (2, 0, 3), (2, 4, 6),
        (3, 0, 2), (3, 3, 6),
        (4, 0, 1), (4, 2, 6),
        (5, 1, 6),
    ],
};

/// Glyph colour for the hover-revealed marks.
///
/// A dark wash of the dot's own hue rather than pure black — that is
/// how the system draws it, and pure black on the yellow dot reads far
/// heavier than on the red one.
pub const GLYPH_FG: [f32; 4] = [0.12, 0.10, 0.06, 0.80];

/// Standard colours (close to Apple HIG values).
pub const COLOR_CLOSE: [f32; 4] = [0.99, 0.36, 0.31, 1.0];
pub const COLOR_MIN:   [f32; 4] = [0.99, 0.74, 0.18, 1.0];
pub const COLOR_MAX:   [f32; 4] = [0.21, 0.78, 0.35, 1.0];

impl TrafficLights {
    /// Lay out 3 dots inside `title_bar`, anchored to its left.
    /// `size` is the diameter in physical px; `gap` is the
    /// horizontal spacing between dots; `left_pad` is the inset
    /// from the title bar's left edge to the close dot.
    pub fn layout(title_bar: Rect, size: f64, gap: f64, left_pad: f64) -> Self {
        let y = title_bar.y_top + (title_bar.h - size) * 0.5;
        let x0 = title_bar.x + left_pad;
        let x1 = x0 + size + gap;
        let x2 = x1 + size + gap;
        Self {
            close: Rect { x: x0, y_top: y, w: size, h: size },
            min:   Rect { x: x1, y_top: y, w: size, h: size },
            max:   Rect { x: x2, y_top: y, w: size, h: size },
        }
    }

    /// Push 3 `UiRectInstance` dots into `ui_rects`.  Caller has
    /// already drawn the title-bar BG.
    pub fn paint(&self, ui_rects: &mut Vec<UiRectInstance>, hovered: bool) {
        paint_discs(ui_rects, &[self.close, self.min, self.max], hovered);
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<TrafficLightHit> {
        if self.close.contains(x, y) { return Some(TrafficLightHit::Close); }
        if self.min.contains(x, y)   { return Some(TrafficLightHit::Minimize); }
        if self.max.contains(x, y)   { return Some(TrafficLightHit::Maximize); }
        None
    }
}

/// Draw the three discs, plus the hover marks when `hovered`.
///
/// One drawing path, taking the rects from whoever owns the geometry.
/// There used to be two — this component, and an inline copy in the
/// Process Monitor painter — and six rounds of "make these match the
/// system" went into editing whichever one happened to be wrong at the
/// time.  Everything here is a rect, so it needs no painter: the marks
/// are mask runs, not glyphs.
pub fn paint_discs(
    ui_rects: &mut Vec<UiRectInstance>,
    rects: &[Rect; 3],
    hovered: bool,
) {
    let plain = |r: Rect, color: [f32; 4], radius: f32| UiRectInstance {
        origin: [r.x as f32, r.y_top as f32],
        size: [r.w as f32, r.h as f32],
        fill_color: color,
        border_color: [0.0; 4],
        corner_radius: radius,
        // No rim: the shader strokes borders *inside* the shape, so
        // 1 px eats a pixel off every edge of an already-small disc.
        border_width: 0.0,
        shadow_blur: 0.0,
        shadow_alpha: 0.0,
        shadow_color: [0.0, 0.0, 0.0, 1.0],
    };
    for (r, color, icon) in [
        (rects[0], COLOR_CLOSE, &ICON_CLOSE),
        (rects[1], COLOR_MIN, &ICON_MIN),
        (rects[2], COLOR_MAX, &ICON_ZOOM),
    ] {
        ui_rects.push(plain(r, color, (r.w * 0.5) as f32));
        if !hovered {
            continue;
        }
        // Whole-pixel units and origins — a fractional cell smears each
        // run across two rows and the mark reads blurry and low.
        let unit = (r.w * icon.extent / icon.grid_w).round().max(1.0);
        let ox = (r.x + (r.w - unit * icon.grid_w) * 0.5).round();
        let oy = (r.y_top + (r.h - unit * icon.grid_h) * 0.5).round();
        for (row, x0, x1) in icon.runs {
            ui_rects.push(plain(
                Rect {
                    x: ox + unit * (*x0 as f64),
                    y_top: oy + unit * (*row as f64),
                    w: unit * ((x1 - x0) as f64),
                    h: unit,
                },
                GLYPH_FG,
                0.0,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn layout_anchors_close_at_left() {
        let bar = rect(100.0, 50.0, 800.0, 28.0);
        let t = TrafficLights::layout(bar, LIGHT_SIZE_LOGICAL, LIGHT_GAP_LOGICAL, LIGHT_LEFT_PAD_LOGICAL);
        // Assertions derive from the constants rather than restating
        // their current values — the previous version hardcoded both the
        // inputs and the expected outputs, so bumping the diameter broke
        // a test that was only ever checking arithmetic.
        assert_eq!(t.close.x, 100.0 + LIGHT_LEFT_PAD_LOGICAL);
        // Vertically centered in the title bar.
        assert_eq!(t.close.y_top, 50.0 + (28.0 - LIGHT_SIZE_LOGICAL) * 0.5);
        // Stride between dots is diameter + gap.
        let stride = LIGHT_SIZE_LOGICAL + LIGHT_GAP_LOGICAL;
        assert_eq!(t.min.x - t.close.x, stride);
        assert_eq!(t.max.x - t.min.x, stride);
    }

    #[test]
    fn hit_test_disjoint_regions() {
        let t = TrafficLights::layout(
            rect(0.0, 0.0, 800.0, 28.0),
            LIGHT_SIZE_LOGICAL, LIGHT_GAP_LOGICAL, LIGHT_LEFT_PAD_LOGICAL,
        );
        let mid_close = (t.close.x + t.close.w * 0.5, t.close.y_top + t.close.h * 0.5);
        let mid_min   = (t.min.x   + t.min.w   * 0.5, t.min.y_top   + t.min.h   * 0.5);
        let mid_max   = (t.max.x   + t.max.w   * 0.5, t.max.y_top   + t.max.h   * 0.5);
        assert_eq!(t.hit_test(mid_close.0, mid_close.1), Some(TrafficLightHit::Close));
        assert_eq!(t.hit_test(mid_min.0,   mid_min.1),   Some(TrafficLightHit::Minimize));
        assert_eq!(t.hit_test(mid_max.0,   mid_max.1),   Some(TrafficLightHit::Maximize));
        assert_eq!(t.hit_test(-1.0, -1.0), None);
    }
}
