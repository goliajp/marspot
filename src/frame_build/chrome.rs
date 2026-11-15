//! The window's chrome, laid out as instances: the seams between
//! panes and the focus ring and the empty seats, and the sidebar, the
//! toolbar and the version label, which are drawn from trees.
//!
//! Sizes are `spec::chrome`'s and colours `panel_palette`'s; where a
//! thing is, is the layout's.  A click and the pointer are tested
//! against the trees' kept layouts (`ChromeSurfaces::at`), so what
//! lights and what is pressed are the nodes the frame was drawn from.

use crate::layout::{Layout, Rect};
use crate::render::SidebarEntry;
use crate::frame_build::color::rgba8_of_f32;
use golia_ui_core::scene::UiRectInstance;
use crate::font_cache::FontCache;
use crate::ui::components::chrome_target::ChromeTarget;
use crate::ui::components::title_strip_view::Strips;
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
    pub(crate) titles: crate::ui::components::title_strip_view::TitleStripSurface,
    /// The panes' rectangles for the seams, refilled every frame.
    seam_rects: Vec<Rect>,
    /// The layout generation and row count the toolbar and the sidebar
    /// were last made ready for input with.
    ready_for: (u64, usize),
}

/// What the chrome shows beyond where things are.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChromeState {
    /// A panel or a menu lies over the sidebar: nothing on it lights.
    pub(crate) quiet: bool,
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
    paint_grid_seams(painter, layout, n_views, focused_idx, overlay_ui_rects, &mut surfaces.seam_rects);
    paint_empty_seats(painter, layout, n_views);
    let cell_w = painter.cell_w;
    surfaces.sidebar.frame(painter, &SidebarFrame { layout, rows: &sidebar, focused_idx, quiet: state.quiet, cell_w });
    paint_region_seams(painter, layout);
    let cell = (painter.cell_w, painter.cell_h, painter.ascent);
    surfaces.toolbar.frame(painter, &Toolbar::of(layout, state.on, cell));
}

/// What the chrome's trees are laid out from when input arrives
/// between frames: where things are, how many rows the sidebar lists,
/// and the panes' strips (only their badges are read).
pub struct ChromeShape<'a> {
    pub layout: &'a Layout,
    pub rows: usize,
    pub strips: &'a dyn Strips,
}

impl ChromeSurfaces {
    /// Bring each tree's layout to where the next frame will draw it.
    fn ready(&mut self, font: &mut FontCache, s: &ChromeShape<'_>) {
        self.toolbar.ready(font, s.layout);
        self.sidebar.ready(font, s.layout, s.rows);
        self.titles.ready(font, s.layout, s.strips);
    }

    /// What `p` is on in the layout the trees were last laid out to;
    /// the toolbar first, then the sidebar, then the strips.
    pub(crate) fn hit(&self, p: (f64, f64)) -> Option<ChromeTarget> {
        [&self.toolbar.kept, &self.sidebar.kept, &self.titles.kept]
            .into_iter()
            .find_map(|k| k.surface.hit(p))
            .and_then(|h| ChromeTarget::of(h.action))
    }

    /// What `p` is on.
    pub(crate) fn at(&mut self, font: &mut FontCache, s: &ChromeShape<'_>, p: (f64, f64)) -> Option<ChromeTarget> {
        self.ready(font, s);
        self.hit(p)
    }

    /// The pointer moved to `p`: true when what is lit changed, which
    /// is a picture to paint again and nothing to lay out.
    pub(crate) fn pointer_move(&mut self, font: &mut FontCache, s: &ChromeShape<'_>, p: (f64, f64)) -> bool {
        // the strips light nothing and are not told
        if self.ready_for != (s.layout.generation, s.rows) {
            self.ready_for = (s.layout.generation, s.rows);
            self.toolbar.ready(font, s.layout);
            self.sidebar.ready(font, s.layout, s.rows);
        }
        let toolbar = self.toolbar.kept.surface.pointer_move(p);
        // on a toolbar button, the pointer is on nothing in the sidebar
        let sidebar = if self.toolbar.kept.surface.interaction.hovered.is_some() {
            self.sidebar.kept.surface.pointer_leave()
        } else {
            self.sidebar.kept.surface.pointer_move(p)
        };
        toolbar | sidebar
    }

    /// The pointer left the window: nothing is lit.
    pub(crate) fn pointer_leave(&mut self) -> bool {
        let toolbar = self.toolbar.kept.surface.pointer_leave();
        self.sidebar.kept.surface.pointer_leave() | toolbar
    }

    /// A press at `p`: each tree remembers what it landed on, and the
    /// answer is what that is.
    pub(crate) fn press(&mut self, font: &mut FontCache, s: &ChromeShape<'_>, p: (f64, f64)) -> Option<ChromeTarget> {
        self.ready(font, s);
        for k in [&mut self.toolbar.kept, &mut self.sidebar.kept, &mut self.titles.kept] {
            k.surface.pointer_down(p);
        }
        self.hit(p)
    }

    /// The button let go at `p`, or, with `None`, somewhere no tree of
    /// this window is: what was pressed, if it is also what `p` is on.
    pub(crate) fn release(
        &mut self,
        font: &mut FontCache,
        s: &ChromeShape<'_>,
        p: Option<(f64, f64)>,
    ) -> Option<ChromeTarget> {
        self.ready(font, s);
        let mut released = None;
        for k in [&mut self.toolbar.kept, &mut self.sidebar.kept, &mut self.titles.kept] {
            let action = match p {
                Some(p) => k.surface.pointer_up(p),
                None => {
                    k.surface.pointer_cancel();
                    None
                }
            };
            released = released.or(action);
        }
        released.and_then(ChromeTarget::of)
    }
}

fn paint_grid_seams(
    painter: &mut ViewPainter,
    layout: &Layout,
    n_views: usize,
    focused_idx: usize,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
    pane_rects: &mut Vec<Rect>,
) {
    // Seams first, then the ring: both are plain fills in submission
    // order, after the pane backgrounds, so the ring replaces the seam
    // pixel for pixel on the sides of the pane in use.
    if layout.gutter > 0.0 && !layout.cells.is_empty() {
        let seam_thickness = layout.gutter * chrome::SEAM_GUTTERS;
        let seam_style = SeamStyle { color: palette::seam(), thickness: seam_thickness };
        pane_rects.clear();
        pane_rects.extend(layout.cells.iter().map(|c| Rect { x: c.x, y_top: c.y_top, w: c.w, h: c.h }));
        GridSeams {
            cells: pane_rects,
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
