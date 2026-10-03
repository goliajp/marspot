//! A whole frame laid out as instances: the panes, the chrome around
//! them, and the overlays on top.

use crate::font_cache::FontCache;
use crate::frame_build::chrome::paint_chrome;
use crate::frame_build::pane_cache::{push_panes, PaneInstanceCache};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::Layout;
use crate::render::{SessionView, SidebarEntry};
use crate::frame_build::color::rgba8_of_f32;
use golia_ui_core::scene::{RectInstance as CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::cc_usage_modal::CcUsageRender;
use crate::ui::components::context_menu_paint::ContextMenuRender;
use crate::ui::components::layout_modal_paint::LayoutModalRender;
use crate::ui::components::process_panel::ProcessPanelRender;
use crate::ui::components::settings_paint::SettingsRender;
use crate::ui::core::view::ViewPainter;

/// What `build_instances` measured about itself.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BuildStats {
    pub(crate) panes_us: u64,
    pub(crate) rebuilt: u32,
    pub(crate) considered: u32,
}

/// Lay out one frame -- every pane, the chrome around them, and the
/// overlays on top -- into scratch vecs the caller keeps from frame to
/// frame.
pub(crate) fn build_instances(
    layout: &Layout,
    views: &[SessionView],
    sidebar: &[SidebarEntry],
    focused_idx: usize,
    window_focused: bool,
    hover_chrome_btn: Option<u8>,
    process_panel: Option<&ProcessPanelRender>,
    cc_usage: Option<&CcUsageRender>,
    settings_panel: Option<&SettingsRender>,
    layout_modal_state: Option<&LayoutModalRender>,
    drop_preview: Option<((f64, f64, f64, f64), bool)>,
    drag_source: Option<usize>,
    context_menu_state: Option<&ContextMenuRender>,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    _dots: &mut Vec<CellInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
    pane_caches: &mut Vec<PaneInstanceCache>,
    overlay_cells: &mut Vec<CellInstance>,
    overlay_glyphs: &mut Vec<GlyphInstance>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) -> BuildStats {
    let cell_w = font.cell_w as f32;
    let cell_h = font.cell_h as f32;
    let ascent = font.ascent as f32;
    let (atlas_w, atlas_h) = atlas.dims();
    let atlas_w_f = atlas_w as f32;
    let atlas_h_f = atlas_h as f32;

    push_drop_preview(drop_preview, overlay_ui_rects);
    push_scrims(layout, views, drag_source, overlay_ui_rects);

    // Sidebar BG = cell BG (already covered by the clear pass).  No
    // explicit fill needed unless the sidebar palette ever diverges.

    // Every internal hairline — sidebar↔grid, header↔grid, and
    // cell↔cell — uses one uniformly weak SEAM tone at one width.
    // The eye reads structure (this is a sidebar / this is a grid /
    // this is a cell) without any seam taking on chrome weight.
    // F3+1.18 — GridSeams now uses the BG (cells) pipeline so seams
    // tile pixel-perfect with no SDF AA seams.  This means push
    // order matters (BG pass renders in instance order): GridSeams
    // MUST run AFTER pane BG cells (push_session in pane loop), so
    // seams cover pane BG instead of being overdrawn by it, which is
    // why `paint_chrome` runs after `push_panes`.

    let stats = push_panes(
        layout, views, window_focused, cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
        font, atlas, color_atlas, cells, glyphs, color_glyphs, ui_rects, pane_caches,
    );

    // Everything after the panes in the main layers goes through one
    // painter: seams and focus ring, empty seats, sidebar, toolbar,
    // close and add buttons, version label.
    {
        let mut painter = ViewPainter {
            cell_w, cell_h, ascent,
            atlas_w: atlas_w_f, atlas_h: atlas_h_f,
            window_w: layout.window_w, window_h: layout.window_h,
            font, atlas, cells, glyphs, ui_rects,
        };
        paint_chrome(
            &mut painter, layout, views.len(), sidebar, focused_idx, hover_chrome_btn,
            overlay_ui_rects,
        );
    }

    paint_overlays(
        layout, views, process_panel, cc_usage, settings_panel, layout_modal_state,
        cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
        font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
    );

    // F3+9 / P2c — right-click ContextMenu render is NOT routed
    // through the shared overlay scratches; it builds a `Canvas`
    // in render_layout AFTER the main encode_passes and uses
    // `encode_canvas` to draw in submission-order.  The variable
    // is consumed there.
    let _ = context_menu_state;

    stats
}

fn push_drop_preview(
    drop_preview: Option<((f64, f64, f64, f64), bool)>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    // RFC-006 — drop-preview ghost: a translucent accent fill + thin
    // accent frame over the region a hovering pane drag would occupy
    // on release; outline-only for an Append landing (no half-pane to
    // promise, just "into this window").  Overlay pass, above the
    // content it previews.  What lights up is exactly what release
    // does.
    if let Some(((gx, gy, gw, gh), outline_only)) = drop_preview {
        let accent = crate::ui::theme::token::color::ACCENT.to_rgba_f32();
        let fill = if outline_only {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            [accent[0] * 0.25, accent[1] * 0.25, accent[2] * 0.25, 0.25]
        };
        overlay_ui_rects.push(UiRectInstance {
            origin: [gx as f32, gy as f32],
            size: [gw as f32, gh as f32],
            fill: rgba8_of_f32(fill),
            border: rgba8_of_f32(accent),
            radius: 4.0,
            border_width: 2.0,
            shadow_offset: [0.0, 0.0],
            shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
            shadow_blur: 0.0,
        });
    }
}

fn push_scrims(
    layout: &Layout,
    views: &[SessionView],
    drag_source: Option<usize>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    // RFC-006 polish — whole-cell scrims, one primitive two duties:
    //  * dormant placeholder: a steady recess so an empty seat reads
    //    as "space held, nothing running" (hint text shows through);
    //  * drag source: a deeper dim while its pane is mid-drag —
    //    "you are moving THIS one" (the ⇢ title marker stays for the
    //    pointer-outside-any-window case).
    for (i, view) in views.iter().enumerate() {
        // Reasons a pane can recede, one primitive.  The deepest wins
        // rather than stacking: two scrims at 0.22 read as one at
        // 0.39, which is a different (and unintended) shade.
        let dimming = [
            (drag_source == Some(i)).then_some(DRAG_SOURCE_SCRIM),
            view.dormant.then_some(EMPTY_SEAT_SCRIM),
            // Already eased by the pane — see `ScrimFade`.  The step
            // it is heading for is `attention_scrim(focused, recede)`;
            // what arrives here is where it has got to.
            Some(view.scrim),
        ]
        .into_iter()
        .flatten()
        .filter(|a| *a > 0.0)
        .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a: f32| a.max(v))));
        let (Some(alpha), Some(rect)) = (dimming, layout.cells.get(i)) else {
            continue;
        };
        overlay_ui_rects.push(UiRectInstance {
            origin: [rect.x as f32, rect.y_top as f32],
            size: [rect.w as f32, rect.h as f32],
            fill: rgba8_of_f32([0.0, 0.0, 0.0, alpha]),
            border: golia_ui_core::Rgba8::TRANSPARENT,
            radius: 0.0,
            border_width: 0.0,
            shadow_offset: [0.0, 0.0],
            shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
            shadow_blur: 0.0,
        });
    }
}

/// The search overlay over each pane that has one open, then any
/// modal panel.
fn paint_overlays(
    layout: &Layout,
    views: &[SessionView],
    process_panel: Option<&ProcessPanelRender>,
    cc_usage: Option<&CcUsageRender>,
    settings_panel: Option<&SettingsRender>,
    layout_modal_state: Option<&LayoutModalRender>,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w_f: f32,
    atlas_h_f: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    overlay_cells: &mut Vec<CellInstance>,
    overlay_glyphs: &mut Vec<GlyphInstance>,
    overlay_ui_rects: &mut Vec<UiRectInstance>,
) {
    // F3+1.8 — per-pane search overlay.  Painted via the same
    // overlay-scratch path as the modal — no filter, no in-cache
    // mixing, opaque BG by default via `ViewStyle`.  Position is
    // pane-local, sourced from the layout cell rect we already
    // walked to push the session's grid.
    {
        use crate::ui::core::view::ViewPainter;
        use crate::ui::components::search_overlay::{
            paint_search_overlay, SearchOverlayParams,
        };
        for (i, view) in views.iter().enumerate() {
            let Some(rect) = layout.cells.get(i) else { continue };
            let Some(overlay) = view.search_overlay.as_ref() else { continue };
            let inner_x = rect.x as f32 + layout.padding as f32;
            let inner_y = rect.y_top as f32
                + layout.cell_title_h as f32
                + layout.padding as f32;
            let mut painter = ViewPainter {
                cell_w, cell_h, ascent,
                atlas_w: atlas_w_f, atlas_h: atlas_h_f,
                window_w: layout.window_w, window_h: layout.window_h,
                font, atlas,
                cells: overlay_cells,
                glyphs: overlay_glyphs,
                ui_rects: overlay_ui_rects,
            };
            paint_search_overlay(&mut painter, SearchOverlayParams {
                overlay,
                inner_x,
                inner_y,
                grid_cols: view.grid.cols(),
                grid_rows: view.grid.rows(),
            });
        }
    }

    // F3+1.7 — Process Monitor modal renders via the `View` component
    // which owns the overlay-scratch + extra-pass plumbing.  Build
    // sites only see the painter; backdrop / frame / always-on-top
    // are configured up front and applied automatically.
    if let Some(panel) = process_panel {
        crate::ui::components::process_panel::push_process_panel_via_view(
            panel,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // cc — `Cc` usage modal (Claude account windows).  Same overlay
    // + backdrop treatment as the process panel.
    if let Some(cc) = cc_usage {
        crate::ui::components::cc_usage_paint::push_cc_usage_via_view(
            cc,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // The settings panel.  Same overlay + backdrop treatment.
    if let Some(sp) = settings_panel {
        crate::ui::components::settings_paint::push_settings_panel_via_view(
            sp,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }

    // F3+3.0 — LayoutModal: cols/rows steppers + Apply.  Overlay
    // scratches → renders on top of grid, modal-style backdrop dims
    // everything below the title strip.
    if let Some(modal_state) = layout_modal_state {
        crate::ui::components::layout_modal_paint::push_layout_modal_via_view(
            modal_state,
            layout.top_inset,
            cell_w, cell_h, ascent, atlas_w_f, atlas_h_f,
            layout.window_w, layout.window_h,
            font, atlas, overlay_cells, overlay_glyphs, overlay_ui_rects,
        );
    }
}

/// The attention ladder: how present a pane is, from whether the user
/// is in it and how far its session has receded.
///
/// One pane is fully there — the one being used.  Everything else
/// steps back; a session that has been sitting finished steps back
/// again; one that has been reclaimed steps back furthest.  A glance
/// should answer "where am I / what is still alive / what has drifted
/// out" before any reading happens.
///
/// Expressed as scrim alpha, which is `1 - opacity`: 0.25 scrim =
/// 75 % opacity, 0.50 = 50 %, 0.75 = 25 %.
pub(crate) const UNFOCUSED_SCRIM: f32 = 0.25;
pub(crate) const RESTING_SCRIM: f32 = 0.50;
pub(crate) const PARKED_SCRIM: f32 = 0.75;
/// Deeper than either, because dragging is a live gesture and the
/// source pane has to read as "the one in your hand".
pub(crate) const DRAG_SOURCE_SCRIM: f32 = 0.38;
/// A seat a pane moved out of — nothing is running there, and the
/// hint text has to stay readable through it.
pub(crate) const EMPTY_SEAT_SCRIM: f32 = 0.22;

/// Scrim for a pane from the attention ladder alone.
///
/// The focused pane is never dimmed at any level: whatever the state
/// machine thinks of it, a pane the user is looking at is not
/// receding from where they sit.
/// The dim a pane's attention level calls for.
///
/// The *target*, not what is painted: `Pane`'s `ScrimFade` eases
/// towards it, and the view carries the eased value.
pub fn attention_scrim(focused: bool, recede: u32) -> f32 {
    if focused {
        return 0.0;
    }
    let rung = match recede {
        0 => UNFOCUSED_SCRIM,
        1 => RESTING_SCRIM,
        _ => PARKED_SCRIM,
    };
    // The *ladder* is not a setting — its order says what marspot knows
    // about each pane.  How loudly it says it is: a wide grid wants
    // less, a pair of panes wants more.  Read per call, which is once
    // per pane per frame — the cheap end of `get()`, unlike the
    // per-byte path that had to grow an atomic mirror.
    (rung * crate::settings::get().dim_scale).clamp(0.0, 0.92)
}
