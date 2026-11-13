//! How the process monitor's tables are painted.
//!
//! Everything is drawn where the frame's `Layout` says it is; nothing
//! here works out a position of its own.  Which row and which frame
//! is drawn when is the panel's tree (`process_view`).

use marspot_term::layout::Rect;

use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::components::process_panel::{
    col, format_rss, pane_columns, process_columns, PANES_HEADING, PROCESSES_HEADING,
};
use crate::ui::components::process_view::ProcessPanelRender;
use crate::ui::components::table::{Align, TableGeometry};
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, content, space};

/// Below this a process counts as idle: it would print as `0.0`.
const IDLE_CPU_PCT: f32 = 0.05;

/// The fill behind a row: the selected one in the accent, the one
/// under the pointer in the hover colour.
fn paint_row_state(p: &mut ViewPainter, g: &TableGeometry, row: Rect, selected: bool, hovered: bool) {
    if selected {
        g.paint_highlight(p, row, palette::row_selected());
    } else if hovered {
        g.paint_highlight(p, row, palette::row_hover());
    }
}

/// The left group's heading and the frame of its table.
pub(crate) fn paint_panes_frame(p: &mut ViewPainter, panel: &ProcessPanelRender) {
    let l = &panel.layout;
    let h = l.panes_heading;
    panel_chrome::section_heading(p, h.x, h.top, h.w, t(PANES_HEADING), None);
    l.panes.paint_frame(p, &pane_columns());
}

/// Pane `i`'s row of the left table, at `row`.
pub(crate) fn paint_pane_row(p: &mut ViewPainter, panel: &ProcessPanelRender, i: usize, row: Rect, hovered: bool) {
    let g = &panel.layout.panes;
    let Some(pane) = panel.pane_rows.get(i) else { return };
    let selected = i == panel.selected_pane;
    paint_row_state(p, g, row, selected, hovered);
    let number = if selected { palette::fg() } else { palette::fg_sec() };
    g.paint_cell(p, row, col::NAME, Align::Left, 0, &pane.name, palette::fg());
    g.paint_cell(p, row, col::PANE_PIDS, Align::Right, 0, &pane.n_pids.to_string(), number);
    g.paint_cell(p, row, col::PANE_CPU, Align::Right, 0, &format!("{:.1}", pane.cpu_pct), number);
    g.paint_cell(p, row, col::PANE_RSS, Align::Right, 0, &format_rss(pane.rss_kb), number);
}

/// The right group's heading, with which pane it is about, and the
/// frame of its table.
pub(crate) fn paint_processes_frame(p: &mut ViewPainter, panel: &ProcessPanelRender) {
    let l = &panel.layout;
    let h = l.processes_heading;
    let detail = fit_heading_trailing(p, &panel.detail, h.w);
    panel_chrome::section_heading(p, h.x, h.top, h.w, t(PROCESSES_HEADING), Some(&detail));
    // Only the headers are read here; the kill column has none, and
    // its width is already in the layout.
    l.processes.paint_frame(p, &process_columns(0.0));
}

/// Process `i`'s row of the right table, at `row`.
pub(crate) fn paint_process_row(p: &mut ViewPainter, panel: &ProcessPanelRender, i: usize, row: Rect, hovered: bool) {
    let g = &panel.layout.processes;
    let Some(process) = panel.rows.get(i) else { return };
    paint_row_state(p, g, row, false, hovered);
    g.paint_cell(p, row, col::NAME, Align::Left, process.depth, &process.comm, palette::fg());
    g.paint_cell(p, row, col::PID, Align::Right, 0, &process.pid.to_string(), palette::fg_sec());
    // An idle process shows a dot, so the busy ones stand out of the
    // column.
    if process.cpu_pct > IDLE_CPU_PCT {
        let cpu = format!("{:.1}", process.cpu_pct);
        g.paint_cell(p, row, col::CPU, Align::Right, 0, &cpu, palette::fg_sec());
    } else {
        g.paint_cell(p, row, col::CPU, Align::Right, 0, "·", palette::fg_muted());
    }
    g.paint_cell(p, row, col::RSS, Align::Right, 0, &format_rss(process.rss_kb), palette::fg_sec());
}

/// `text` cut, with an ellipsis, to what a heading `w` wide has left
/// beside its title.
fn fit_heading_trailing(p: &mut ViewPainter, text: &str, w: f64) -> String {
    let title_w = p.panel_text_width(content::HEADING, t(PROCESSES_HEADING)) as f64;
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
