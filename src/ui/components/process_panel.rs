//! The Process Monitor modal: the data a frame of it is drawn from,
//! and how it is painted.
//!
//! Painted through a `ViewPainter` into the overlay scratches, so the
//! modal is above everything else in the frame by construction.

use marspot_term::layout::{Alignment, Rect};

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use golia_ui_core::scene::{RectInstance as CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::modal_frame::{MODAL_FRAME_BG, MODAL_FRAME_BORDER, MODAL_FRAME_CORNER_RADIUS};

/// F3+4 — one row in the detail process-tree panel (right column of
/// the redesigned Process Monitor).  Pure data; L2 builds these every
/// frame for the selected pane.  Includes per-pid resource cols so
/// the renderer can lay out a proper sortable table.
#[derive(Debug, Clone)]
pub struct ProcessPanelRow {
    /// Indent depth (0 = pane header / shell-root, 1+ = tree
    /// descendants).  Cosmetic only — the leftmost column.
    pub depth: u8,
    /// Process pid.  0 if `is_header` (panel header row).
    pub pid: i32,
    /// Process comm (16 chars max on macOS — what `ps` shows).
    pub comm: String,
    /// Sampled CPU% (delta of two task_info samples / wall-clock dt
    /// × 100).  May exceed 100 for multi-threaded procs (a `rustc`
    /// pinning one P-core reads ~100; a parallel `cargo build` walks
    /// up to ncpu × 100).  0 for header rows.
    pub cpu_pct: f32,
    /// Resident set size in KB at the latest sample.
    pub rss_kb: u64,
    /// True when this is the synthetic header row for the pane (no
    /// kill button drawn, slightly heavier FG).  False for tree rows.
    pub is_header: bool,
}

/// F3+4 — one row in the MASTER pane list (left column of the
/// redesigned Process Monitor).  Each row summarises one pane: name +
/// aggregate process count / CPU% / RSS over the pane's pid tree.
#[derive(Debug, Clone)]
pub struct ProcessPanelPaneRow {
    /// Pane name as the user sees it elsewhere — custom title >
    /// cwd basename > ordinal.  Already truncated by L2 to fit the
    /// master column width.
    pub name: String,
    pub sid: u64,
    /// Number of pids in the pane's tree (including the shell root).
    pub n_pids: u32,
    /// Sum of `ProcessPanelRow.cpu_pct` across the tree.
    pub cpu_pct: f32,
    /// Sum of `ProcessPanelRow.rss_kb` across the tree.
    pub rss_kb: u64,
    /// "What's busy" hint: comm of the top non-shell process by CPU%,
    /// or "(idle)" / "" when the pane is quiet.  Pre-truncated.
    pub busy: String,
}

/// F3+1.4 — full data for one render of the centered Process Monitor
/// modal.  Renderer pulls this via `set_process_panel`.  `None` =
/// closed, nothing drawn.
#[derive(Debug, Clone)]
pub struct ProcessPanelRender {
    /// Modal frame rectangle (physical px), centered by L2 over the
    /// window.  Includes title bar + master/detail body.
    pub rect: Rect,
    /// Title bar text — currently always "Process Monitor".  Kept
    /// a String so a future plugin could rename per-pane modals.
    pub title: String,
    /// F3+4 — MASTER column: one row per pane (left side).
    /// Pre-sorted by L2; renderer paints in order.  Empty Vec
    /// (no live panes) draws an "(no panes)" placeholder.
    pub pane_rows: Vec<ProcessPanelPaneRow>,
    /// Which pane row in `pane_rows` is currently selected; the
    /// `rows` field below is the process tree of that pane.  Out-
    /// of-range silently clamped to 0 by the renderer.
    pub selected_pane: usize,
    /// F3+4 — DETAIL column: rows of the selected pane's process
    /// tree (header + flatten_pre_order).  Empty Vec OK.
    pub rows: Vec<ProcessPanelRow>,
    /// F3+1.5 — collapse body so only the title bar paints.  When
    /// `true`, `pane_rows` + `rows` are NOT rendered (still walked
    /// for hit-rect bookkeeping on the L2 side).
    pub minimized: bool,
    /// The three traffic-light discs, in physical px.
    ///
    /// Computed once in `marspot-core` (which owns the real backing
    /// scale) and drawn verbatim here.  The painter used to recompute
    /// them from `scale_hint`, a *font-derived* factor — so the dots
    /// scaled with the terminal's point size while the hit rects scaled
    /// with the display, and the two disagreed: 12 px drawn against a
    /// 15 px click target.  System chrome tracks the system, not the
    /// font.
    pub light_rects: [Rect; 3],
    /// Cursor is over the panel's title bar.
    ///
    /// macOS reveals the ×/−/+ glyphs inside its window buttons while
    /// the pointer is anywhere in the title bar — not just over one
    /// button — so the affordance is "these are clickable", not "this
    /// one is". Mirrored here.
    pub title_bar_hovered: bool,
    /// F3+1.5 — body scroll offset (physical px) for the DETAIL
    /// column.  Renderer paints rows starting at
    /// `body_top + body_pad_top - scroll_y`, clipped at body bottom.
    pub scroll_y: f64,
    /// F3+1.5 — semi-transparent backdrop covering the rest of the
    /// window so the modal reads as focused.  Painted as one
    /// `UiRectInstance` before the modal frame.
    pub draw_backdrop: bool,
}

const PROCESS_PANEL_TITLE_BAR_BG: [f32; 4] = [0.18, 0.19, 0.23, 1.0];
const PROCESS_PANEL_SEPARATOR: [f32; 4] = [0.06, 0.07, 0.09, 1.0];
const PROCESS_PANEL_TITLE_FG: [f32; 4] = [0.92, 0.94, 0.97, 1.0];
const PROCESS_PANEL_TAB_BG_ACTIVE: [f32; 4] = [0.20, 0.30, 0.55, 1.0];
const PROCESS_PANEL_TAB_FG_ACTIVE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const PROCESS_PANEL_ROW_FG: [f32; 4] = [0.86, 0.88, 0.92, 1.0];
const PROCESS_PANEL_HEADER_FG: [f32; 4] = [0.65, 0.78, 0.95, 1.0];

const PROCESS_PANEL_TITLE_BAR_H_LOGICAL: f32 = 28.0;
const PROCESS_PANEL_TAB_STRIP_H_LOGICAL: f32 = 30.0;
// Traffic-light geometry and colour live in
// `ui::system::macos::traffic_lights` — the panel used to carry its own
// copy of both, which is why two rounds of "make the dots match the
// system" edited constants this painter never read.
/// Inset between the panel's frame and its content on every side.
///
/// The tables used to be laid out from the panel's own edges, so the
/// first column sat on the left border, the kill buttons sat on the
/// right one, and rows ran to the bottom edge with nothing under them —
/// the content read as spilling out of its frame.  One value for all
/// four sides, same rule the cc modal's cards follow.
pub const PROCESS_PANEL_SIDE_PAD_LOGICAL: f32 = 10.0;
/// Fraction of the padded content width given to the master (pane)
/// table; the detail table gets the rest.
///
/// Public because the hit-test geometry is computed a second time in
/// `marspot-core` — the two derivations have to agree or the kill
/// buttons stop landing where they are drawn, so at minimum they share
/// the numbers.
pub const PROCESS_PANEL_MASTER_FRAC: f64 = 0.38;
const PROCESS_PANEL_KILL_W_LOGICAL: f32 = 18.0;

/// F3+1.7 — paint the Process Monitor modal via the `View` component.
/// The modal's frame chrome (BG, border, shadow, backdrop) is owned
/// by `View::paint`; only the modal-specific content (title bar fill,
/// traffic lights, tabs, body rows) lives in the closure below.  All
/// drawing routes through `ViewPainter` into overlay scratches, so
/// the modal is always-on-top by construction — no filter pass on
/// the grid scratches needed.
pub(crate) fn push_process_panel_via_view(
    panel: &ProcessPanelRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: panel.rect,
        style: ViewStyle {
            bg: MODAL_FRAME_BG,
            border_color: MODAL_FRAME_BORDER,
            border_width: 1.0,
            corner_radius: MODAL_FRAME_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: if panel.draw_backdrop {
                Backdrop::Dim {
                    color: [0.0, 0.0, 0.0, 0.45],
                    exclude_above_y: top_inset,
                }
            } else {
                Backdrop::None
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_process_panel_content(panel, p);
    });
}

/// Internal: the content of the Process Monitor modal (title bar
/// fill, traffic lights, title text, tab strip, body rows + [×]
/// kill buttons).  Called from `push_process_panel_via_view` with
/// a `ViewPainter` routing to overlay scratches.
fn paint_process_panel_content(
    panel: &ProcessPanelRender,
    p: &mut crate::ui::core::view::ViewPainter,
) {
    let px = panel.rect.x as f32;
    let py = panel.rect.y_top as f32;
    let pw = panel.rect.w as f32;
    let ph = panel.rect.h as f32;
    // The original push_process_panel inferred scale from cell_h;
    // keep that heuristic so glyph sizes stay in proportion.
    let scale_hint = (p.cell_h / 20.0).max(0.5);
    let title_h = PROCESS_PANEL_TITLE_BAR_H_LOGICAL * scale_hint;
    let _tab_h = PROCESS_PANEL_TAB_STRIP_H_LOGICAL * scale_hint;
    use crate::ui::system::macos::traffic_lights as tl;


    // Title bar fill (flat over the rounded chrome).
    p.fill_rect(
        Rect { x: px as f64, y_top: py as f64, w: pw as f64, h: title_h as f64 },
        PROCESS_PANEL_TITLE_BAR_BG,
    );
    // 1 px separator below title bar.
    p.fill_rect(
        Rect { x: px as f64, y_top: (py + title_h) as f64, w: pw as f64, h: 1.0 },
        PROCESS_PANEL_SEPARATOR,
    );
    // Traffic lights (3 SDF discs anchored title-bar left).

    tl::paint_discs(p.ui_rects, &panel.light_rects, panel.title_bar_hovered);
    // Centred title text, in the shared panel role — a panel's name
    // is set the same way in every panel.  The body below stays mono:
    // it is a process table, and columns line up for free there.
    p.panel_text_in(
        crate::ui::theme::PanelText::Title,
        Rect { x: px as f64, y_top: py as f64, w: pw as f64, h: title_h as f64 },
        &panel.title,
        PROCESS_PANEL_TITLE_FG,
        Alignment::Center,
    );

    if panel.minimized {
        return;
    }

    // F3+4.1 — master / detail rendered via the new `Table`
    // component.  The kill [×] is layered on top of the detail
    // Table after paint() since Table is text-only.
    use crate::ui::components::TableStyle;

    let pad = PROCESS_PANEL_SIDE_PAD_LOGICAL * scale_hint;
    let body_top = py + title_h + pad;
    let body_bottom = py + ph - pad;
    // Content width is the frame minus a pad on each outer edge; the
    // master/detail split lands inside that, not on the frame.
    let content_x = px + pad;
    let content_w = pw - pad * 2.0;
    let master_w = content_w * PROCESS_PANEL_MASTER_FRAC as f32;
    let split_x = content_x + master_w;

    // Vertical separator between master and detail spans full body.
    p.fill_rect(
        Rect { x: split_x as f64, y_top: body_top as f64,
               w: 1.0, h: (body_bottom - body_top) as f64 },
        PROCESS_PANEL_SEPARATOR,
    );

    // Shared Table style with marspot's panel palette.
    let style = TableStyle {
        header_h:           (p.cell_h * 1.4) as f64,
        row_h:              (p.cell_h * 1.3) as f64,
        indent_px:          12.0 * scale_hint as f64,
        col_pad:            10.0 * scale_hint as f64,
        header_bg:          PROCESS_PANEL_TITLE_BAR_BG,
        header_fg:          PROCESS_PANEL_HEADER_FG,
        header_separator:   PROCESS_PANEL_SEPARATOR,
        row_bg:             [0.0; 4],
        row_bg_alt:         [0.0; 4],
        row_bg_selected:    PROCESS_PANEL_TAB_BG_ACTIVE,
        row_fg:             PROCESS_PANEL_ROW_FG,
        row_fg_selected:    PROCESS_PANEL_TAB_FG_ACTIVE,
        section_fg:         PROCESS_PANEL_HEADER_FG,
    };

    let geo = BodyGeometry {
        scale_hint, pad, body_top, body_bottom, content_x, content_w, master_w, split_x,
    };
    paint_master_table(panel, p, &geo, style);
    paint_detail_table(panel, p, &geo, style);
}

/// Where the two tables go, worked out once for both.
#[derive(Clone, Copy)]
struct BodyGeometry {
    scale_hint: f32,
    pad: f32,
    body_top: f32,
    body_bottom: f32,
    content_x: f32,
    content_w: f32,
    master_w: f32,
    split_x: f32,
}

/// The master table: one row per pane.
fn paint_master_table(
    panel: &ProcessPanelRender,
    p: &mut crate::ui::core::view::ViewPainter,
    geo: &BodyGeometry,
    style: crate::ui::components::TableStyle,
) {
    use crate::ui::components::{ColumnWidth, RowKind, Table, TableColumn, TableRow};
    let BodyGeometry { pad, body_top, body_bottom, content_x, master_w, .. } = *geo;
    let m_cols = vec![
        TableColumn {
            header: "Pane".into(),
            width: ColumnWidth::Flex(1.0),
            align: Alignment::CenterLeft,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "pids".into(),
            width: ColumnWidth::Px((p.cell_w * 6.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "CPU%".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: Some(crate::ui::components::SortDir::Desc),
            sortable: false,
        },
        TableColumn {
            header: "RSS".into(),
            width: ColumnWidth::Px((p.cell_w * 8.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
    ];
    let m_rows: Vec<TableRow> = panel.pane_rows.iter().map(|r| TableRow {
        cells: vec![
            r.name.clone(),
            r.n_pids.to_string(),
            format!("{:.1}", r.cpu_pct),
            format_rss(r.rss_kb),
        ],
        depth: 0,
        kind: RowKind::Data,
    }).collect();
    let master_table = Table {
        rect: Rect {
            x: content_x as f64, y_top: body_top as f64,
            w: (master_w - pad * 0.5) as f64,
            h: (body_bottom - body_top) as f64,
        },
        columns: &m_cols,
        rows: &m_rows,
        style,
        selected: Some(panel.selected_pane),
        scroll_y: 0.0,
        show_header: true,
    };
    master_table.paint(p);
}

/// The detail table -- the selected pane's process tree -- and the
/// kill [×] buttons laid over its last column.
fn paint_detail_table(
    panel: &ProcessPanelRender,
    p: &mut crate::ui::core::view::ViewPainter,
    geo: &BodyGeometry,
    style: crate::ui::components::TableStyle,
) {
    use crate::ui::components::{
        Button, ButtonStyle, ColumnWidth, IconPosition, IconSpec, RowKind, Table, TableColumn, TableRow,
    };
    let BodyGeometry { scale_hint, pad, body_top, body_bottom, content_w, master_w, split_x, .. } = *geo;
    let d_cols = vec![
        TableColumn {
            header: "Process".into(),
            width: ColumnWidth::Flex(1.0),
            align: Alignment::CenterLeft,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "pid".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "CPU%".into(),
            width: ColumnWidth::Px((p.cell_w * 7.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "RSS".into(),
            width: ColumnWidth::Px((p.cell_w * 8.0) as f64),
            align: Alignment::CenterRight,
            sort: None, sortable: false,
        },
        TableColumn {
            header: "".into(),
            width: ColumnWidth::Px(
                (PROCESS_PANEL_KILL_W_LOGICAL * scale_hint + 8.0) as f64,
            ),
            align: Alignment::Center,
            sort: None, sortable: false,
        },
    ];
    let d_rows: Vec<TableRow> = panel.rows.iter().map(|r| TableRow {
        cells: if r.is_header {
            vec![r.comm.clone(), String::new(), String::new(), String::new(), String::new()]
        } else {
            vec![
                r.comm.clone(),
                r.pid.to_string(),
                if r.cpu_pct > 0.05 { format!("{:.1}", r.cpu_pct) } else { "·".into() },
                format_rss(r.rss_kb),
                String::new(),
            ]
        },
        depth: r.depth,
        kind: if r.is_header { RowKind::Section } else { RowKind::Data },
    }).collect();
    let detail_table = Table {
        rect: Rect {
            x: (split_x + pad * 0.5) as f64, y_top: body_top as f64,
            w: (content_w - master_w - pad * 0.5) as f64,
            h: (body_bottom - body_top) as f64,
        },
        columns: &d_cols,
        rows: &d_rows,
        style,
        selected: None,
        scroll_y: panel.scroll_y,
        show_header: true,
    };
    detail_table.paint(p);

    // Kill buttons overlay the Table's last column for non-header rows.
    let kill_w = PROCESS_PANEL_KILL_W_LOGICAL * scale_hint;
    let kill_h = (style.row_h as f32 - 4.0).max(8.0);
    let col_xw = detail_table.column_x_widths();
    let kill_col = col_xw.last().copied();
    if let Some((kx, kw)) = kill_col {
        for (i, row) in panel.rows.iter().enumerate() {
            if row.is_header { continue; }
            let row_rect = detail_table.row_rect(i);
            if row_rect.y_top + row_rect.h <= detail_table.body_rect().y_top { continue; }
            if row_rect.y_top >= detail_table.body_rect().y_top
                + detail_table.body_rect().h { break; }
            let bx = kx + (kw - kill_w as f64) * 0.5;
            let by = row_rect.y_top + (row_rect.h - kill_h as f64) * 0.5;
            let btn = Button {
                rect: Rect { x: bx, y_top: by, w: kill_w as f64, h: kill_h as f64 },
                label: None,
                icon: Some(IconSpec::Glyph("×")),
                icon_position: IconPosition::Only,
                hovered: false,
                style: ButtonStyle::destructive(),
            };
            btn.paint(p);
        }
    }
}

/// F3+4 — pretty-print RSS bytes (KB units) as a `X.Y M` / `X.Y G`
/// style human-readable string for the master/detail rss column.
/// Pre-formatted by L2 so the renderer doesn't pull format helpers.
fn format_rss(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    } else if kb >= 1024 {
        format!("{:.0}M", kb as f64 / 1024.0)
    } else {
        format!("{}K", kb)
    }
}
