//! The window's chrome, laid out as instances: the seams between
//! panes and the focus ring, empty seats, the sidebar, the toolbar and
//! the version label.
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
    Outline, SeamStyle, Sidebar,
};
use crate::ui::core::view::ViewPainter;
use crate::ui::system::macos::icons::{self, Symbol};
use crate::ui::theme::spec::chrome;
use crate::ui::SESSION_COUNT_HARD_CAP;

/// What the chrome shows beyond where things are.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChromeState {
    pub(crate) hover: Option<ChromeHover>,
    /// Per toolbar button, in `Layout::toolbar_buttons` order: whether
    /// what it opens is open.  The dev panel is another window, owned
    /// by another process, so its button never reads as on.
    pub(crate) on: [bool; 6],
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
) {
    paint_grid_seams(painter, layout, n_views, focused_idx, overlay_ui_rects);
    paint_empty_seats(painter, layout, n_views);
    paint_sidebar(painter, layout, sidebar, focused_idx, state.hover);
    paint_region_seams(painter, layout);
    paint_toolbar(painter, layout, state);
    paint_version_label(painter, layout);
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
        let seam_thickness = (layout.gutter * chrome::SEAM_GUTTERS).max(2.0);
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

fn paint_sidebar(
    painter: &mut ViewPainter,
    layout: &Layout,
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    hover: Option<ChromeHover>,
) {
    if layout.sidebar_w <= 0.0 {
        return;
    }
    // The add button: a ghost — its `+`, and a fill only under the
    // pointer.  At the pane limit it is drawn disabled and a click on
    // it does nothing.
    let add = layout.add_session_button_rect;
    if add.w > 0.0 {
        let full = layout.close_session_rects.len() >= SESSION_COUNT_HARD_CAP;
        let hovered = hover == Some(ChromeHover::Add) && !full;
        if hovered {
            // As wide as a row's fill, so the two line up; the button
            // itself is where the layout put it.
            let fill = Rect {
                x: chrome::SIDEBAR_ROW_INSET,
                w: (layout.sidebar_w - 2.0 * chrome::SIDEBAR_ROW_INSET).max(0.0),
                ..add
            };
            painter.fill_rounded_rect(
                fill, palette::chrome_hover(), chrome::SIDEBAR_ROW_RADIUS as f32, (palette::none(), 0.0),
            );
        }
        let fg = if full {
            palette::disabled_fg()
        } else if hovered {
            palette::fg()
        } else {
            palette::chrome_icon()
        };
        icons::paint(painter, Symbol::Plus, add, chrome::SIDEBAR_ADD_ICON, fg);
    }
    Sidebar {
        rect: Rect {
            x: 0.0,
            y_top: layout.top_inset,
            w: layout.sidebar_w,
            h: (layout.window_h - layout.top_inset).max(0.0),
        },
        top_pad: layout.sidebar_top_pad_phys,
        rows: sidebar,
        close_rects: &layout.close_session_rects,
        focused_idx,
        hover,
    }
    .paint(painter);
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

fn paint_toolbar(p: &mut ViewPainter, layout: &Layout, state: ChromeState) {
    // Every toolbar button, painted from one list.
    //
    // `layout.toolbar_buttons()` is the layout's own order, zipped
    // against a same-length icon array — so a button the layout knows
    // about cannot be left unpainted.  The settings button was added
    // without this and shipped invisible: the rect was laid out and
    // the hit-test worked, so clicking the empty space opened a panel
    // nobody could see a button for.
    //
    // A hidden sidebar's button is the symbol at rest, like any button
    // whose panel is closed; showing, it is on.
    let symbols = [
        Symbol::Sidebar,
        Symbol::Layout,
        Symbol::Processes,
        Symbol::DevPanel,
        Symbol::Usage,
        Symbol::Settings,
    ];
    for (i, (rect, icon)) in layout.toolbar_buttons().into_iter().zip(symbols).enumerate() {
        // A zero-width rect is a button this layout does not show
        // (snapshot / bench paths build a chrome-less layout).
        if rect.w <= 0.0 {
            continue;
        }
        Button {
            rect,
            label: None,
            icon: Some(icon),
            icon_position: IconPosition::Only,
            hovered: state.hover == Some(ChromeHover::Toolbar(i as u8)),
            style: ButtonStyle::toolbar(state.on[i]),
        }
        .paint(p);
    }
}

fn paint_version_label(painter: &mut ViewPainter, layout: &Layout) {
    let (cell_w, cell_h, ascent) = (painter.cell_w, painter.cell_h, painter.ascent);
    // Quiet metadata flush right, on the same row as the traffic
    // lights and the toolbar buttons.
    if layout.top_inset > 0.0 {
        let label = version_label();
        let text_w = label.chars().count() as f32 * cell_w;
        let scale = (layout.top_inset / crate::HEADER_PT) as f32;
        let x = (layout.window_w as f32 - chrome::VERSION_PAD_RIGHT as f32 * scale - text_w).max(cell_w);
        let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32 * scale;
        let baseline_y = (row_center - cell_h * 0.5).max(0.0) + ascent;
        painter.text(x, baseline_y, &label, palette::chrome_icon());
    }
}

/// The running binary's version label, e.g. `v0.12.67`.  Bumped on
/// every commit that changes a binary (project rule), so it still works
/// as visible proof that a silent update landed — the product name and
/// the git sha that used to trail it were noise in a corner the user
/// reads dozens of times a day.  The sha stays available in the window
/// title and in logs.
pub fn version_label() -> String {
    // L2 (marspot-core) is the canonical marspot version — the header
    // Two numbers, because two questions are being asked.  The
    // product version says which marspot this is; the core build
    // number says whether the code someone just wrote is the code
    // running, which is the only reason that number exists.
    crate::version_line()
}
