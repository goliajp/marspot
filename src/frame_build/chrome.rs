//! The window's chrome, laid out as instances: the seams between
//! panes and the focus ring, empty seats, the sidebar, the toolbar,
//! the close and add buttons, and the version label.

use crate::frame_build::palette::*;
use crate::layout::{Layout, Rect};
use crate::render::SidebarEntry;
use crate::render_metal::{rgba8_of_f32, UiRectInstance};
use crate::session::SessionState;
use crate::ui::components::{
    Button, ButtonStyle, GridEdges, GridItem, GridSeams, IconPosition, IconSpec, Outline,
    SeamStyle, Sidebar, SidebarRow, SidebarStyle,
};
use crate::ui::core::view::ViewPainter;

/// Everything drawn over the panes and under the overlays, in the
/// order it stacks.
pub(crate) fn paint_chrome(
    painter: &mut ViewPainter,
    layout: &Layout,
    n_views: usize,
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    hover_chrome_btn: Option<u8>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    paint_grid_seams(painter, layout, n_views, focused_idx, overlay_ui_rects);
    paint_empty_seats(painter, layout, n_views);
    paint_sidebar(painter, layout, sidebar, focused_idx);
    // Toolbar buttons + picker overlay + close/add BGs.
    push_layout_chrome(layout, hover_chrome_btn, painter);
    paint_button_glyphs(painter, layout);
    paint_version_label(painter, layout);
}

fn paint_grid_seams(
    painter: &mut ViewPainter,
    layout: &Layout,
    n_views: usize,
    focused_idx: usize,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    // F3+1.19 — grid base seams FIRST (gray dividers between
    // panes), then per-pane GridItem outline.  Both go through
    // the BG pipeline (fill_rect, no SDF AA), so push order =
    // z order: pane BG (pushed by pane loop above) → base seams
    // → focus outline on the focused pane.  All boundaries are
    // pixel-perfect — rasterizer assigns each pixel to one rect
    // by sample-center rule, no AA seams.
    if layout.gutter > 0.0 && !layout.cells.is_empty() {
        // F3+1.14 — pane grid seams thickened to 4× gutter so the
        // grid reads as deliberate panes, not gutters-as-margin.
        let seam_thickness = (layout.gutter * 4.0).max(2.0);
        let seam_style = SeamStyle {
            color: [SEAM.0, SEAM.1, SEAM.2, 1.0],
            thickness: seam_thickness,
        };
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

        // F3+1.20 — per-pane focus outline.  RN-style outline
        // (doesn't shrink pane content), width + gutter passed
        // so GridItem places the 8 ring rects at exactly where
        // the GridSeams base seams sit — outline pixel-for-pixel
        // REPLACES the gray seam color on the focused side, no
        // overshoot beyond the seam footprint.
        let focus_outline = Outline {
            color: [0.72, 0.76, 0.82, 1.0],
            width: seam_thickness,
        };
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
    let cell_h = painter.cell_h;
    // Empty-cell BG overlay + [+] hint.  Each empty cell becomes
    // a low-key ghost Button with a "+" glyph centered inside.
    let n_visible = n_views;
    let empty_style = ButtonStyle {
        bg:           [EMPTY_CELL_OVERLAY[0], EMPTY_CELL_OVERLAY[1],
                       EMPTY_CELL_OVERLAY[2], EMPTY_CELL_OVERLAY[3]],
        bg_hover:     [EMPTY_CELL_OVERLAY[0], EMPTY_CELL_OVERLAY[1],
                       EMPTY_CELL_OVERLAY[2], EMPTY_CELL_OVERLAY[3]],
        fg:           EMPTY_CELL_GLYPH_FG,
        fg_hover:     EMPTY_CELL_GLYPH_FG,
        border_color: [0.0, 0.0, 0.0, 0.0],
        border_width: 0.0,
        corner_radius: 0.0,
        padding_x: 0.0,
        icon_gap: 0.0,
        icon_size: cell_h,
    };
    for cell_rect in layout.cells.iter().skip(n_visible) {
        let inner_top = cell_rect.y_top + layout.cell_title_h;
        let inner_h = (cell_rect.h - layout.cell_title_h).max(0.0);
        let inner = Rect {
            x: cell_rect.x, y_top: inner_top,
            w: cell_rect.w, h: inner_h,
        };
        Button {
            rect: inner,
            label: None,
            icon: Some(IconSpec::Glyph("+")),
            icon_position: IconPosition::Only,
            hovered: false,
            style: empty_style,
        }.paint(painter);
    }
}

fn paint_sidebar(painter: &mut ViewPainter, layout: &Layout, sidebar: &[SidebarEntry], focused_idx: usize) {
    // Sidebar — rows + status dot + label.  Replaces push_sidebar.
    if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
        let rows: Vec<SidebarRow> = sidebar.iter().map(|e| {
            let dot = match e.state {
                SessionState::Active => STATE_ACTIVE,
                SessionState::Idle   => STATE_IDLE,
                SessionState::Exited => STATE_EXITED,
            };
            SidebarRow {
                label: e.label,
                dot_color: [dot.0, dot.1, dot.2, 1.0],
            }
        }).collect();
        Sidebar {
            rect: Rect {
                x: 0.0,
                y_top: layout.top_inset,
                w: layout.sidebar_w,
                h: (layout.window_h - layout.top_inset).max(0.0),
            },
            rows: &rows,
            focused_idx,
            style: SidebarStyle {
                focused_bg: [BG_FOCUSED.0, BG_FOCUSED.1, BG_FOCUSED.2, 1.0],
                label_fg:   [SIDEBAR_TEXT_FG.0, SIDEBAR_TEXT_FG.1, SIDEBAR_TEXT_FG.2, 1.0],
                row_h: SIDEBAR_ROW_H,
                top_pad: layout.sidebar_top_pad_phys as f32,
                left_pad: SIDEBAR_LEFT_PAD,
                dot_radius: SIDEBAR_DOT_R,
                dot_label_gap: SIDEBAR_DOT_LABEL_GAP,
            },
        }
        .paint(painter);
    }
}

fn paint_button_glyphs(painter: &mut ViewPainter, layout: &Layout) {
    let (cell_w, cell_h, ascent) = (painter.cell_w, painter.cell_h, painter.ascent);
    // Close-[×] glyph per close button — `painter.text("×", ...)`
    // centred in each close_session_rect.  Replaces push_close_glyphs.
    if !layout.close_session_rects.is_empty() {
        let close_disabled = layout.close_session_rects.len() == 1;
        let color = if close_disabled { CLOSE_BTN_FG_DISABLED } else { CLOSE_BTN_FG };
        for r in &layout.close_session_rects {
            let x = (r.x + (r.w - cell_w as f64) * 0.5) as f32;
            let y_baseline = (r.y_top + (r.h - cell_h as f64) * 0.5) as f32 + ascent;
            painter.text(x, y_baseline, "×", color);
        }
    }
    // Add-[+] glyph in the sidebar add button.  Replaces
    // push_add_button_glyph.
    if layout.add_session_button_rect.w > 0.0 {
        let add_disabled =
            layout.close_session_rects.len() >= SESSION_COUNT_HARD_CAP;
        let color = if add_disabled { ADD_BTN_FG_DISABLED } else { ADD_BTN_FG };
        let r = layout.add_session_button_rect;
        let x = (r.x + (r.w - cell_w as f64) * 0.5) as f32;
        let y_baseline = (r.y_top + (r.h - cell_h as f64) * 0.5) as f32 + ascent;
        painter.text(x, y_baseline, "+", color);
    }
}

fn paint_version_label(painter: &mut ViewPainter, layout: &Layout) {
    let (cell_w, cell_h, ascent) = (painter.cell_w, painter.cell_h, painter.ascent);
    // Header version label — quiet metadata flush right, on the
    // same row as the traffic lights and the toolbar buttons.
    if layout.top_inset > 0.0 {
        let label = version_label();
        let text_w = label.chars().count() as f32 * cell_w;
        let scale_approx = (layout.top_inset as f32) / crate::HEADER_PT as f32;
        let right_margin_logical_pt: f32 = 10.0;
        let right_margin_phys = right_margin_logical_pt * scale_approx;
        let x =
            ((layout.window_w as f32) - right_margin_phys - text_w).max(cell_w);
        let row_center = marspot_term::layout::TRAFFIC_LIGHT_CENTER_Y_LOGICAL as f32
            * scale_approx;
        let baseline_y = (row_center - cell_h * 0.5).max(0.0) + ascent;
        painter.text(
            x, baseline_y, &label,
            [HEADER_VERSION_FG.0, HEADER_VERSION_FG.1, HEADER_VERSION_FG.2, 1.0],
        );
    }
}

/// Empty-cell BG tint.  Painted over `layout.cells[views.len()..]`
/// when sessions count is less than layout cell count so the
/// "open slot" reads as a softer, slightly elevated area.  Low-
/// alpha black-ish overlay over the existing cell BG keeps the
/// terminal palette intact.
const EMPTY_CELL_OVERLAY: [f32; 4] = [0.04, 0.05, 0.07, 0.6];
const EMPTY_CELL_GLYPH_FG: [f32; 4] = [0.30, 0.34, 0.38, 0.85];

// Close-[×] button colours.  Subtle red-tinted BG so the user
// reads "destructive action zone" without it screaming; the
// actual `×` glyph is rasterised via the FG pass so it's a
// true diagonal cross (NOT the axis-aligned `+` an earlier
// attempt drew, which read as "add" — wrong affordance).
// When this is the only session (close_session_rects.len() == 1)
// a softer gray pair is used so the user sees the affordance
// is disabled.
const CLOSE_BTN_BG: [f32; 4] = [0.18, 0.07, 0.08, 0.85];
const CLOSE_BTN_FG: [f32; 4] = [0.92, 0.62, 0.62, 1.0];
const CLOSE_BTN_BG_DISABLED: [f32; 4] = [0.10, 0.10, 0.11, 0.65];
const CLOSE_BTN_FG_DISABLED: [f32; 4] = [0.45, 0.45, 0.47, 0.8];

// Add-[+] button colours.  Green-tinted BG to read as
// "constructive action".  Disabled (N == SESSION_COUNT_HARD_CAP)
// drops to gray — `mouse_down` ignores the click but the dim look
// explains why.
const ADD_BTN_BG: [f32; 4] = [0.07, 0.16, 0.10, 0.85];
const ADD_BTN_FG: [f32; 4] = [0.65, 0.92, 0.72, 1.0];
const ADD_BTN_BG_DISABLED: [f32; 4] = [0.10, 0.10, 0.11, 0.65];
const ADD_BTN_FG_DISABLED: [f32; 4] = [0.45, 0.45, 0.47, 0.8];

// RFC-004 E.1 (B11) — this was a private `= 9` that predated the
// cap raise to 36 in ui::mod; the [+] button greyed out at 9 panes
// while spawn paths honoured 36.  One constant, one truth.
use crate::ui::SESSION_COUNT_HARD_CAP;

fn push_layout_chrome(
    layout: &Layout,
    hover_chrome_btn: Option<u8>,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    use crate::ui::components::{Button, ButtonStyle, IconSpec, IconPosition};
    use crate::ui::system::macos::icons::{SidebarIcon, GridIcon, ListTreeIcon, DevPanelIcon, UsageBarsIcon, SlidersIcon};

    // F3+1.12 — chrome hairline seams (sidebar↔grid + header↔grid).
    // Same SEAM tone as GridSeams; routed through the painter (UI
    // pipeline) so they layer over pane BG like the rest of the
    // chrome.  There used to be a third seam splitting the header into
    // title strip + toolbar; the header is one band now, so drawing a
    // line through it would cut the row the lights sit on.
    if layout.gutter > 0.0 {
        let g = layout.gutter;
        let seam = [SEAM.0, SEAM.1, SEAM.2, 1.0];
        let ui_fill = |p: &mut crate::ui::core::ViewPainter, r: Rect| {
            p.fill_rounded_rect(r, seam, 0.0, ([0.0, 0.0, 0.0, 0.0], 0.0));
        };
        if layout.sidebar_w > 0.0 {
            ui_fill(p, Rect {
                x: layout.sidebar_w, y_top: layout.top_inset,
                w: g, h: layout.window_h - layout.top_inset,
            });
        }
        if layout.top_inset > 0.0 {
            ui_fill(p, Rect {
                x: 0.0, y_top: layout.top_inset - g,
                w: layout.window_w, h: g,
            });
        }
    }

    // Every toolbar button, painted from one list.
    //
    // `layout.toolbar_buttons()` is the layout's own order, zipped
    // against a same-length icon array — so a button the layout knows
    // about cannot be left unpainted.  The settings button was added
    // without this and shipped invisible: the rect was laid out and
    // the hit-test worked, so clicking the empty space opened a panel
    // nobody could see a button for.
    let sidebar_collapsed = layout.sidebar_w == 0.0;
    let sidebar_icon = SidebarIcon { collapsed: sidebar_collapsed };
    let grid_icon = GridIcon { cols: layout.grid_cols, rows: layout.grid_rows };
    let list_tree_icon = ListTreeIcon;
    let dev_panel_icon = DevPanelIcon;
    let usage_icon = UsageBarsIcon;
    let sliders_icon = SlidersIcon;
    let icons: [&dyn crate::ui::core::IconComponent; 6] = [
        &sidebar_icon,
        &grid_icon,
        &list_tree_icon,
        &dev_panel_icon,
        &usage_icon,
        &sliders_icon,
    ];
    let chrome = ButtonStyle::chrome();
    for (i, (rect, icon)) in layout.toolbar_buttons().into_iter().zip(icons).enumerate() {
        // A zero-width rect is a button this layout does not show
        // (snapshot / bench paths build a chrome-less layout).
        if rect.w <= 0.0 {
            continue;
        }
        let btn = Button {
            rect,
            label: None,
            icon: Some(IconSpec::Component(icon)),
            icon_position: IconPosition::Only,
            hovered: hover_chrome_btn == Some(i as u8),
            style: chrome,
        };
        btn.paint(p);
    }

    // F3+3.0 — picker popup removed; `LayoutModal` (separate
    // component) replaces it.

    // Sidebar close-[×] BG tints (FG `×` glyph laid down later in
    // `push_close_glyphs`).  Painted as plain rounded rects via the
    // UI pipeline so they layer correctly on top of sidebar BG.
    let n_sessions = layout.close_session_rects.len();
    let close_disabled = n_sessions == 1;
    let close_bg = if close_disabled { CLOSE_BTN_BG_DISABLED } else { CLOSE_BTN_BG };
    for rect in &layout.close_session_rects {
        p.fill_rounded_rect(*rect, close_bg, 0.0,
            ([0.0, 0.0, 0.0, 0.0], 0.0));
    }
    // Sidebar [+] add-session BG.  Same pipeline reasoning.
    if layout.add_session_button_rect.w > 0.0 {
        let add_disabled = n_sessions >= SESSION_COUNT_HARD_CAP;
        let add_bg = if add_disabled { ADD_BTN_BG_DISABLED } else { ADD_BTN_BG };
        p.fill_rounded_rect(layout.add_session_button_rect, add_bg, 0.0,
            ([0.0, 0.0, 0.0, 0.0], 0.0));
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
