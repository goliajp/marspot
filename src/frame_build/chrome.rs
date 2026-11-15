//! The window's chrome, laid out as instances: the seams between
//! panes and the focus ring and the empty seats, and the sidebar, the
//! toolbar and the version label, which are drawn from trees.
//!
//! Sizes are `spec::chrome`'s and colours `panel_palette`'s; where a
//! thing is, and so where a click lands on it, is the layout's.

use crate::layout::{Layout, Rect};
use crate::render::SidebarEntry;
use crate::frame_build::color::rgba8_of_f32;
use golia_ui_core::scene::UiRectInstance;
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::{
    Button, ButtonStyle, GridEdges, GridItem, GridSeams, IconPosition,
    Outline, SeamStyle,
};
use crate::ui::core::view::ViewPainter;
use crate::ui::system::macos::icons::Symbol;
use crate::ui::theme::spec::chrome;
use crate::ui::components::sidebar_view::{SidebarFrame, SidebarSurface};
use crate::ui::components::toolbar_view::{Toolbar, ToolbarSurface};

/// The parts of the chrome drawn from trees, kept from frame to frame.
#[derive(Default)]
pub(crate) struct ChromeSurfaces {
    pub(crate) toolbar: ToolbarSurface,
    pub(crate) sidebar: SidebarSurface,
}

/// What the chrome shows beyond where things are.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChromeState {
    pub(crate) hover: Option<ChromeHover>,
    /// Per toolbar button, in `Layout::toolbar_buttons` order: whether
    /// what it opens is open.
    pub(crate) on: [bool; 5],
}

/// Everything drawn over the panes and under the overlays, in the
/// order it stacks.
pub(crate) fn paint_chrome(
    painter: &mut ViewPainter,
    layout: &Layout,
    n_views: usize,
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    state: ChromeState,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
    surfaces: &mut ChromeSurfaces,
) {
    paint_grid_seams(painter, layout, n_views, focused_idx, overlay_ui_rects);
    paint_empty_seats(painter, layout, n_views);
    let cell_w = painter.cell_w;
    surfaces.sidebar.frame(painter, &SidebarFrame { layout, rows: sidebar, focused_idx, hover: state.hover, cell_w });
    paint_region_seams(painter, layout);
    let cell = (painter.cell_w, painter.cell_h, painter.ascent);
    surfaces.toolbar.frame(painter, &Toolbar::of(layout, state.on, state.hover, cell));
}

fn paint_grid_seams(
    painter: &mut ViewPainter,
    layout: &Layout,
    n_views: usize,
    focused_idx: usize,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    // Seams first, then the ring: both are plain fills in submission
    // order, after the pane backgrounds, so the ring replaces the seam
    // pixel for pixel on the sides of the pane in use.
    if layout.gutter > 0.0 && !layout.cells.is_empty() {
        let seam_thickness = layout.gutter * chrome::SEAM_GUTTERS;
        let seam_style = SeamStyle { color: palette::seam(), thickness: seam_thickness };
        let pane_rects: Vec<Rect> = layout.cells.iter().map(|c| Rect {
            x: c.x, y_top: c.y_top, w: c.w, h: c.h,
        }).collect();
        GridSeams {
            cells: &pane_rects,
            cols: layout.grid_cols,
            rows: layout.grid_rows,
            vertical: seam_style,
            horizontal: seam_style,
        }
        .paint(painter);

        // An outline, not a border: it lies where the seams are and
        // takes nothing from the pane's content.
        let focus_outline = Outline { color: palette::focus_ring(), width: seam_thickness };
        let cols = layout.grid_cols.max(1);
        let rows = layout.grid_rows.max(1);
        for (idx, rect) in pane_rects.iter().enumerate().take(n_views) {
            let r = idx / cols;
            let c = idx % cols;
            let item = GridItem {
                rect: *rect,
                focused: idx == focused_idx,
                outline: focus_outline,
                gutter: layout.gutter,
                edges: GridEdges {
                    top:    r == 0,
                    right:  c == cols - 1,
                    bottom: r == rows - 1,
                    left:   c == 0,
                },
            };
            item.paint(painter);
            // …and again in the overlay pass.  The ring's right and
            // bottom sides sit in the seam, which lies INSIDE the
            // neighbouring cells' rects — and every unfocused pane
            // draws a full-cell scrim in the overlay pass, i.e.
            // after this one.  So the two sides that mark the focus
            // most clearly were the two the neighbours dimmed
            // (2026-08-20 report).  Repainting the same rects after
            // the scrims restores them; the copy is antialiased and
            // the original is not, but they are the same rect in the
            // same colour, so the fringe blends into itself.
            for rr in item.ring_rects() {
                overlay_ui_rects.push(UiRectInstance {
                    origin: [rr.x as f32, rr.y_top as f32],
                    size: [rr.w as f32, rr.h as f32],
                    fill: rgba8_of_f32(focus_outline.color),
                    border: golia_ui_core::Rgba8::TRANSPARENT,
                    radius: 0.0,
                    border_width: 0.0,
                    shadow_offset: [0.0, 0.0],
                    shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
                    shadow_blur: 0.0,
                });
            }
        }
    }
}

fn paint_empty_seats(painter: &mut ViewPainter, layout: &Layout, n_views: usize) {
    // A seat nothing runs in: the window's surface over it and a `+`.
    let style = ButtonStyle {
        bg: palette::empty_seat_bg(),
        bg_hover: palette::empty_seat_bg(),
        fg: palette::empty_seat_fg(),
        fg_hover: palette::empty_seat_fg(),
        corner_radius: 0.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: painter.cell_h,
        ..ButtonStyle::ghost()
    };
    for cell_rect in layout.cells.iter().skip(n_views) {
        let inner = Rect {
            x: cell_rect.x,
            y_top: cell_rect.y_top + layout.cell_title_h,
            w: cell_rect.w,
            h: (cell_rect.h - layout.cell_title_h).max(0.0),
        };
        Button {
            rect: inner,
            label: None,
            icon: Some(Symbol::Plus),
            icon_position: IconPosition::Only,
            hovered: false,
            style,
        }.paint(painter);
    }
}

/// The lines between the sidebar and the grid and under the header, in
/// the seam colour the panes are divided by.  The header itself is one
/// band — the row the window's own buttons sit on — and has no line
/// through it.
fn paint_region_seams(p: &mut ViewPainter, layout: &Layout) {
    let g = layout.gutter;
    if g <= 0.0 {
        return;
    }
    if layout.sidebar_w > 0.0 {
        let r = Rect { x: layout.sidebar_w, y_top: layout.top_inset, w: g, h: layout.window_h - layout.top_inset };
        p.fill_rounded_rect(r, palette::seam(), 0.0, (palette::none(), 0.0));
    }
    if layout.top_inset > 0.0 {
        let r = Rect { x: 0.0, y_top: layout.top_inset - g, w: layout.window_w, h: g };
        p.fill_rounded_rect(r, palette::seam(), 0.0, (palette::none(), 0.0));
    }
}
