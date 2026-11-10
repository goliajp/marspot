//! How the process monitor is painted.
//!
//! Everything is drawn where the frame's `Layout` says it is; nothing
//! here works out a position of its own.

use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance, UiRectInstance};
use marspot_term::layout::Rect;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::components::controls;
use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::process_panel::{
    col, format_rss, pane_columns, process_columns, Hover, ProcessPanelRender, PANES_HEADING,
    PROCESSES_HEADING, TITLE,
};
use crate::ui::components::table::{Align, TableGeometry};
use crate::ui::core::view::ViewPainter;
use crate::ui::theme::spec::{self, content, space};

/// Below this a process counts as idle: it would print as `0.0`.
const IDLE_CPU_PCT: f32 = 0.05;

/// The process monitor, into the overlay scratches: above everything
/// else in the frame.
pub(crate) fn push_process_panel_via_view(
    panel: &ProcessPanelRender,
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
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let close_hovered = panel.hover == Hover::Close;
    panel.layout.chrome.paint(&mut painter, TITLE, None, close_hovered, |p, _content| {
        paint_panes(p, panel);
        paint_processes(p, panel);
    });
}

/// The fill behind a row: the selected one in the accent, the one
/// under the pointer in the hover colour.
fn paint_row_state(p: &mut ViewPainter, g: &TableGeometry, row: Rect, selected: bool, hovered: bool) {
    if selected {
        g.paint_highlight(p, row, palette::row_selected());
    } else if hovered {
        g.paint_highlight(p, row, palette::row_hover());
    }
}

/// The left group: one row per pane.
fn paint_panes(p: &mut ViewPainter, panel: &ProcessPanelRender) {
    let l = &panel.layout;
    let h = l.panes_heading;
    panel_chrome::section_heading(p, h.x, h.top, h.w, PANES_HEADING, None);
    let g = &l.panes;
    g.paint_frame(p, &pane_columns());
    for &(i, row) in &g.rows {
        let Some(pane) = panel.pane_rows.get(i) else { continue };
        let selected = i == panel.selected_pane;
        paint_row_state(p, g, row, selected, panel.hover == Hover::Pane(i));
        let number = if selected { palette::fg() } else { palette::fg_sec() };
        g.paint_cell(p, row, col::NAME, Align::Left, 0, &pane.name, palette::fg());
        g.paint_cell(p, row, col::PANE_PIDS, Align::Right, 0, &pane.n_pids.to_string(), number);
        g.paint_cell(p, row, col::PANE_CPU, Align::Right, 0, &format!("{:.1}", pane.cpu_pct), number);
        g.paint_cell(p, row, col::PANE_RSS, Align::Right, 0, &format_rss(pane.rss_kb), number);
    }
}

/// The right group: the selected pane's process tree, a kill button
/// at the end of every row.
fn paint_processes(p: &mut ViewPainter, panel: &ProcessPanelRender) {
    let l = &panel.layout;
    let h = l.processes_heading;
    let detail = fit_heading_trailing(p, &panel.detail, h.w);
    panel_chrome::section_heading(p, h.x, h.top, h.w, PROCESSES_HEADING, Some(&detail));
    let g = &l.processes;
    // Only the headers are read here; the kill column has none, and
    // its width is already in the layout.
    g.paint_frame(p, &process_columns(0.0));
    for &(i, row) in &g.rows {
        let Some(process) = panel.rows.get(i) else { continue };
        let hovered = matches!(panel.hover, Hover::Process(n) | Hover::Kill(n) if n == i);
        paint_row_state(p, g, row, false, hovered);
        g.paint_cell(p, row, col::NAME, Align::Left, process.depth, &process.comm, palette::fg());
        g.paint_cell(p, row, col::PID, Align::Right, 0, &process.pid.to_string(), palette::fg_sec());
        // An idle process shows a dot, so the busy ones stand out of
        // the column.
        if process.cpu_pct > IDLE_CPU_PCT {
            let cpu = format!("{:.1}", process.cpu_pct);
            g.paint_cell(p, row, col::CPU, Align::Right, 0, &cpu, palette::fg_sec());
        } else {
            g.paint_cell(p, row, col::CPU, Align::Right, 0, "·", palette::fg_muted());
        }
        g.paint_cell(p, row, col::RSS, Align::Right, 0, &format_rss(process.rss_kb), palette::fg_sec());
    }
    for &(i, rect) in &l.kills {
        if i < panel.rows.len() {
            controls::paint_danger_button(p, rect, panel.hover == Hover::Kill(i));
        }
    }
}

/// `text` cut, with an ellipsis, to what a heading `w` wide has left
/// beside its title.
fn fit_heading_trailing(p: &mut ViewPainter, text: &str, w: f64) -> String {
    let title_w = p.panel_text_width(content::HEADING, PROCESSES_HEADING) as f64;
    let room = w - title_w - spec::px(space::S12);
    let fits =
        |p: &mut ViewPainter, s: &str| p.panel_text_width(content::HEADING_TRAILING, s) as f64 <= room;
    if fits(p, text) {
        return text.to_string();
    }
    let mut kept: Vec<char> = text.chars().collect();
    while kept.pop().is_some() {
        let cut: String = kept.iter().chain(std::iter::once(&'…')).collect();
        if fits(p, &cut) {
            return cut;
        }
    }
    String::new()
}
