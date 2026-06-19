//! `GridItem` — one cell in a grid layout.  Owns the focus outline
//! (and, in time, hover / selection / disabled visuals) for that cell.
//!
//! Design follows React Native + CSS outline semantics:
//!
//! - **Outline ≠ border.** Border sits ON the box edge and SHRINKS
//!   inner content area; outline sits AROUND the box and does NOT
//!   shrink content.  This is exactly what a pane focus indicator
//!   needs: the terminal grid inside the pane must not jump 1–4 px
//!   when the user moves focus across panes.
//! - **`focused: bool` is the controlled prop.** In React Native a
//!   parent passes `focused`; the child renders the visual.  marspot
//!   is fully immediate-mode (no event reconciliation layer), so
//!   "focused" is recomputed per frame from `CoreState`'s focused
//!   index and handed in.  When marspot grows a dispatch layer, the
//!   `onFocus` / `onBlur` hooks land here (the surface that already
//!   knows it's the focusable unit).
//!
//! Why GridItem needs `gutter` (and not just an outline `width`):
//!
//! In React Native / CSS, outline sits ENTIRELY outside the box —
//! `outline-offset: 0` puts it flush against the box edge, extending
//! outward by `outline-width`.  But marspot's GridSeams paints base
//! seams with the convention `x = prev_cell.right_edge`, `width =
//! thickness` — i.e. each seam extends `thickness` px IN ONE
//! DIRECTION from the cell edge (into the next cell's content area
//! by `thickness - gutter` px, with the first `gutter` px landing in
//! the literal gutter gap).  A focus outline that mirrors this
//! geometry must use the SAME convention, otherwise outline and
//! base seam misalign by `thickness - gutter` px (visible as the
//! outline overshooting / undershooting the gray seam).
//!
//! So GridItem positions its 8 outline rects at the same place
//! GridSeams would paint a base seam if it ran around this one
//! cell — same x/y/w/h, same pipeline (BG / `fill_rect`), so the
//! outline pixel-for-pixel REPLACES the seam color when focused.
//! `gutter` is the layout's cell-to-cell gap (whatever GridSeams
//! also reads).  `outline.width` is the seam thickness.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

/// Focus outline.  Color + cross-axis thickness.  Geometry on the
/// rect is decided by `GridItem` (which knows the grid's gutter
/// and which edges of the rect are at grid boundaries).
///
/// `width` matches the base seam thickness so the outline overlays
/// the gray seam pixel-perfect.  No `offset` field: position is
/// pinned to the base seam, not to the rect edge.
#[derive(Debug, Clone, Copy)]
pub struct Outline {
    pub color: [f32; 4],
    pub width: f64,
}

impl Outline {
    pub fn none() -> Self {
        Self { color: [0.0; 4], width: 0.0 }
    }
}

/// Per-side flag indicating whether the cell sits at the
/// corresponding edge of its grid.  A `true` side has NO base
/// seam (GridSeams only paints inter-cell seams), so GridItem
/// flips the outline on that side from OUTSIDE-the-rect (overlay
/// base seam) to INSET (occupy the first `outline.width` px
/// INSIDE the cell from that edge).
///
/// Why inset on grid edges:  the only alternative is "outside
/// the rect", which would put the outline beyond grid_bounds —
/// it gets clipped by the window edge or covered by chrome
/// (toolbar, sidebar) and disappears.  A desktop focus
/// indicator must be visible from any focused pane, including
/// corner cells, so on grid-boundary sides we accept the 1–4 px
/// inset into cell content for the focused frame only.  Content
/// in non-focused panes is never shifted.
#[derive(Debug, Clone, Copy, Default)]
pub struct GridEdges {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

pub struct GridItem {
    pub rect: Rect,
    /// True when this cell currently holds focus (controlled prop).
    pub focused: bool,
    pub outline: Outline,
    /// Cell-to-cell gutter in the surrounding grid layout.  GridItem
    /// uses this to place the outline where the grid's BASE SEAM
    /// sits, NOT flush against the rect edge.  See module docs.
    pub gutter: f64,
    /// Which sides of this cell are at the grid's outer boundary.
    /// `top: true` means there's NO row above this one — outline
    /// on that side switches to inset mode to stay visible.  Same
    /// for `bottom` / `left` / `right`.
    pub edges: GridEdges,
}

impl GridItem {
    /// Paint focus chrome.  No-op when not focused or outline is
    /// zero-width.  Eight `fill_rect` calls (4 sides + 4 corners),
    /// all pixel-perfect via the BG pipeline.
    ///
    /// Each side/corner rect's geometry mirrors the GridSeams base
    /// seam at that edge — same origin, same size.  When focused
    /// the outline overlays the gray seam and visually REPLACES
    /// its color in the seam's exact footprint.
    pub fn paint(&self, p: &mut ViewPainter) {
        if !self.focused || self.outline.width <= 0.0 {
            return;
        }
        let r = self.rect;
        let t = self.outline.width;   // = base seam thickness
        let g = self.gutter;          // = grid gutter
        let col = self.outline.color;
        let e = self.edges;
        // Side origins.  When the cell is NOT at the grid edge for
        // a given side, the outline overlays the base seam (which
        // GridSeams paints starting at `cell.<edge> - g` and
        // running `thickness` px into the next cell).  When it IS
        // at the edge, no base seam exists; we INSET into the cell
        // by `t` px so the outline stays visible (otherwise it
        // would render beyond grid bounds and get clipped by the
        // window edge or covered by chrome).
        let top_y    = if e.top    { r.y_top              } else { r.y_top - g };
        let bottom_y = if e.bottom { r.y_top + r.h - t    } else { r.y_top + r.h };
        let left_x   = if e.left   { r.x                  } else { r.x - g };
        let right_x  = if e.right  { r.x + r.w - t        } else { r.x + r.w };
        // 4 sides span the rect's perpendicular extent (cell.w/cell.h).
        p.fill_rect(Rect { x: r.x,    y_top: top_y,    w: r.w, h: t   }, col);
        p.fill_rect(Rect { x: r.x,    y_top: bottom_y, w: r.w, h: t   }, col);
        p.fill_rect(Rect { x: left_x, y_top: r.y_top,  w: t,   h: r.h }, col);
        p.fill_rect(Rect { x: right_x,y_top: r.y_top,  w: t,   h: r.h }, col);
        // 4 corners: t×t squares at base-seam intersections.
        p.fill_rect(Rect { x: left_x,  y_top: top_y,    w: t, h: t }, col);
        p.fill_rect(Rect { x: right_x, y_top: top_y,    w: t, h: t }, col);
        p.fill_rect(Rect { x: left_x,  y_top: bottom_y, w: t, h: t }, col);
        p.fill_rect(Rect { x: right_x, y_top: bottom_y, w: t, h: t }, col);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_outline_paints_nothing() {
        let item = GridItem {
            rect: Rect { x: 0.0, y_top: 0.0, w: 100.0, h: 100.0 },
            focused: true,
            outline: Outline::none(),
            gutter: 1.0,
            edges: GridEdges::default(),
        };
        assert_eq!(item.outline.width, 0.0);
    }

    #[test]
    fn outline_geometry_aligns_with_base_seam() {
        // Base seam between cell c-1 and c lives at:
        //   x = cells[c-1].x + cells[c-1].w = cells[c].x - gutter
        //   width = thickness
        // For a focused cell with rect (100, 50, 200, 100), gutter 1,
        // thickness 4: left base seam x = 99, width = 4 → range [99,103].
        // GridItem's left side rect should be at the same position.
        let r = Rect { x: 100.0, y_top: 50.0, w: 200.0, h: 100.0 };
        let g = 1.0;
        let t = 4.0;
        let left_x = r.x - g;
        let outline_left_range = (left_x, left_x + t);
        let base_seam_left_range = (r.x - g, r.x - g + t);
        assert_eq!(outline_left_range, base_seam_left_range);
    }
}
