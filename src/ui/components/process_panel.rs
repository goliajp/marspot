//! The process monitor: the data a frame of it is drawn from, and
//! where everything in it is.
//!
//! Two groups side by side under the shared panel chrome: the panes on
//! the left, the selected pane's process tree on the right, each a
//! table on a card.
//!
//! [`layout`] is the one place that knows where things are.  The click
//! handler keeps the [`Layout`] of the frame on screen and asks it
//! what the pointer is on; the painter draws that same `Layout` and
//! computes nothing of its own, so a button is clickable exactly where
//! it is drawn.
//!
//! All rects are physical pixels.

use marspot_term::layout::Rect;

use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::surface_stack;
use crate::ui::components::table::{self, Align, Column, ColumnWidth, SortDir, TableGeometry};
use crate::ui::strings::{t, Msg};
use crate::ui::theme::spec::{self, space};

/// The panel's name, in its header.
pub const TITLE: Msg = Msg::PanelProcesses;
pub const PANES_HEADING: Msg = Msg::ProcessPanes;
pub const PROCESSES_HEADING: Msg = Msg::ProcessProcesses;

/// The panel's own measurements.  Widths of table columns are in
/// terminal cells, like everything else about a table.
pub mod metric {
    /// The panel's width: the wide step.
    pub const PANEL_W: f64 = 720.0;
    /// Rows each card has room for.  The height is fixed so the panel
    /// does not resize under the pointer as processes come and go; a
    /// longer process list scrolls.
    pub const VISIBLE_ROWS: usize = 16;
    /// Characters of a pane's name the left card is sized to show.
    pub const PANE_NAME_CELLS: f64 = 22.0;
    /// The most of the content's width the left card may take, however
    /// large the terminal font is.
    pub const PANES_MAX_FRAC: f64 = 0.5;
    /// `pids`: a count of processes.
    pub const COUNT_CELLS: f64 = 4.0;
    /// `▼ CPU%`, and a value up to `1600.0`.
    pub const CPU_CELLS: f64 = 6.0;
    /// `1023M`, `12.3G`.
    pub const RSS_CELLS: f64 = 5.0;
    /// A process id.
    pub const PID_CELLS: f64 = 6.0;
}

/// Column indices of the two tables, for the painter.
pub mod col {
    pub const NAME: usize = 0;
    pub const PANE_PIDS: usize = 1;
    pub const PANE_CPU: usize = 2;
    pub const PANE_RSS: usize = 3;
    pub const PID: usize = 1;
    pub const CPU: usize = 2;
    pub const RSS: usize = 3;
}

/// The left table.  The panes arrive ordered by CPU, busiest first.
pub fn pane_columns() -> [Column; 4] {
    use metric::*;
    [
        Column { header: t(Msg::ColPane), width: ColumnWidth::Flex, align: Align::Left, sort: None },
        Column { header: t(Msg::ColPids), width: ColumnWidth::Cells(COUNT_CELLS), align: Align::Right, sort: None },
        Column {
            header: t(Msg::ColCpu),
            width: ColumnWidth::Cells(CPU_CELLS),
            align: Align::Right,
            sort: Some(SortDir::Desc),
        },
        Column { header: t(Msg::ColRss), width: ColumnWidth::Cells(RSS_CELLS), align: Align::Right, sort: None },
    ]
}

/// The right table.  Its last column holds the kill button, `kill_w`
/// wide.
pub fn process_columns(kill_w: f64) -> [Column; 5] {
    use metric::*;
    [
        Column { header: t(Msg::ColProcess), width: ColumnWidth::Flex, align: Align::Left, sort: None },
        Column { header: t(Msg::ColPid), width: ColumnWidth::Cells(PID_CELLS), align: Align::Right, sort: None },
        Column { header: t(Msg::ColCpu), width: ColumnWidth::Cells(CPU_CELLS), align: Align::Right, sort: None },
        Column { header: t(Msg::ColRss), width: ColumnWidth::Cells(RSS_CELLS), align: Align::Right, sort: None },
        Column { header: "", width: ColumnWidth::Px(kill_w), align: Align::Right, sort: None },
    ]
}

/// One process of the selected pane's tree.
#[derive(Debug, Clone)]
pub struct ProcessPanelRow {
    /// Levels below the pane's shell, which is 0.
    pub depth: u8,
    pub pid: i32,
    /// The command name, as `ps` shows it.
    pub comm: String,
    /// Share of one core over the last sample; above 100 for a process
    /// running on several.
    pub cpu_pct: f32,
    /// Resident memory, in KB.
    pub rss_kb: u64,
}

/// One pane, summed over its process tree.
#[derive(Debug, Clone)]
pub struct ProcessPanelPaneRow {
    /// The pane's name as it appears elsewhere.
    pub name: String,
    pub sid: u64,
    pub n_pids: u32,
    pub cpu_pct: f32,
    pub rss_kb: u64,
    /// The busiest non-shell process, or empty when the pane is quiet.
    pub busy: String,
}

/// Everything one frame of the panel is drawn from.
#[derive(Debug, Clone)]
pub struct ProcessPanelRender {
    /// Where everything is — the same value the click handler holds.
    pub layout: Layout,
    pub pane_rows: Vec<ProcessPanelPaneRow>,
    /// The row of `pane_rows` whose processes are in `rows`.
    pub selected_pane: usize,
    /// Which pane the right table is about: its name, session id and
    /// shell pid, shown at the right of the group's heading.
    pub detail: String,
    pub rows: Vec<ProcessPanelRow>,
    /// What the pointer is on.
    pub hover: Hover,
}

/// What the pointer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hover {
    #[default]
    None,
    /// The header's close button.
    Close,
    /// A row of the left table.
    Pane(usize),
    /// A row of the right table, off its kill button.
    Process(usize),
    /// The kill button of a row of the right table.
    Kill(usize),
}

/// A group heading: its line starts at `top`, from `x`, over `w`.
#[derive(Debug, Clone, Copy)]
pub struct Heading {
    pub x: f64,
    pub top: f64,
    pub w: f64,
}

/// Where everything in the panel is.
#[derive(Debug, Clone)]
pub struct Layout {
    pub chrome: PanelChrome,
    pub panes_heading: Heading,
    pub processes_heading: Heading,
    pub panes: TableGeometry,
    pub processes: TableGeometry,
    /// `(row, rect)` of the kill button of every row of `processes`
    /// that is laid out.
    pub kills: Vec<(usize, Rect)>,
}

/// The panel's frame: its fixed size centred in the window, moved by
/// `offset` — how far its header has been dragged.
pub fn panel_rect(
    window_w: f64,
    window_h: f64,
    cell_h: f64,
    top_inset: f64,
    offset: (f64, f64),
) -> Rect {
    // Whole pixels, here and for the origin below: on a fractional
    // frame the header's text and close button round a pixel away from
    // where every other panel has them.
    let content_h =
        spec::snap_up(panel_chrome::heading_height() + table::card_height(metric::VISIBLE_ROWS, cell_h));
    let r = panel_chrome::frame_rect(
        window_w,
        window_h,
        spec::px(metric::PANEL_W),
        panel_chrome::frame_height(content_h),
        top_inset,
    );
    surface_stack::place(r, offset, window_w, window_h, top_inset)
}

/// Where everything is, for a panel at `frame` showing `n_panes` panes
/// and `n_rows` processes, the process list pushed up by `scroll`.
pub fn layout(
    frame: Rect,
    top_inset: f64,
    n_panes: usize,
    n_rows: usize,
    scroll: f64,
    cell_w: f64,
    cell_h: f64,
) -> Layout {
    let chrome = PanelChrome::new(frame, top_inset);
    let content = chrome.content;
    let gap = spec::px(space::CARD_TO_CARD);
    let (kill_w, kill_h) = controls::danger_button_size_in_row(table::row_height(cell_h));
    let pane_cols = pane_columns();
    let process_cols = process_columns(kill_w);

    // The left card is as wide as its columns; the right one takes the
    // rest, and its name column with it.
    let between = (content.w - gap).max(0.0);
    let left_w = table::natural_width(&pane_cols, metric::PANE_NAME_CELLS, cell_w)
        .min(between * metric::PANES_MAX_FRAC);
    let right_x = content.x + left_w + gap;
    let right_w = between - left_w;

    // On a whole pixel, so the rows under it are too.
    let card_top = spec::snap(content.y_top + panel_chrome::heading_height());
    let card_h = (content.y_top + content.h - card_top).max(0.0);
    let left = Rect { x: content.x, y_top: card_top, w: left_w, h: card_h };
    let right = Rect { x: right_x, y_top: card_top, w: right_w, h: card_h };

    let panes = TableGeometry::new(left, &pane_cols, n_panes, 0.0, cell_w, cell_h);
    let processes = TableGeometry::new(right, &process_cols, n_rows, scroll, cell_w, cell_h);
    // Against the right end of the row's content, centred in the row.
    let kill_right = processes.columns.last().map_or(right.x, |(x, w)| x + w);
    let kills = processes
        .rows
        .iter()
        .map(|&(i, row)| {
            let rect = Rect {
                x: kill_right - kill_w,
                y_top: row.y_top + (row.h - kill_h) * 0.5,
                w: kill_w,
                h: kill_h,
            };
            (i, rect)
        })
        .collect();

    Layout {
        chrome,
        panes_heading: Heading { x: left.x, top: content.y_top, w: left.w },
        processes_heading: Heading { x: right.x, top: content.y_top, w: right.w },
        panes,
        processes,
        kills,
    }
}

impl Layout {
    /// What `(x, y)` is on.  The painter highlights it and a click
    /// acts on it.
    pub fn hover_at(&self, x: f64, y: f64) -> Hover {
        if self.chrome.hit_close(x, y) {
            return Hover::Close;
        }
        if let Some(&(i, _)) = self.kills.iter().find(|(_, r)| r.contains(x, y)) {
            return Hover::Kill(i);
        }
        if let Some(i) = self.panes.row_at(x, y) {
            return Hover::Pane(i);
        }
        self.processes.row_at(x, y).map_or(Hover::None, Hover::Process)
    }

    /// Is `(x, y)` on the header where it is a drag handle?
    pub fn hit_header(&self, x: f64, y: f64) -> bool {
        self.chrome.hit_header(x, y)
    }

    /// Is `(x, y)` anywhere on the panel?
    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.chrome.frame.contains(x, y)
    }

    /// How far the process list is pushed up, and how far it can be.
    pub fn scroll(&self) -> (f64, f64) {
        (self.processes.scroll, self.processes.max_scroll)
    }
}

/// Resident memory for the RSS column: `512K`, `86M`, `1.2G`.
pub fn format_rss(kb: u64) -> String {
    const K: u64 = 1024;
    if kb >= K * K {
        format!("{:.1}G", kb as f64 / (K * K) as f64)
    } else if kb >= K {
        format!("{:.0}M", kb as f64 / K as f64)
    } else {
        format!("{kb}K")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::spec::{card, content, header, table as t};

    const CELL_W: f64 = 7.0;
    const CELL_H: f64 = 16.0;
    const WINDOW: (f64, f64) = (1600.0, 1000.0);

    fn frame() -> Rect {
        panel_rect(WINDOW.0, WINDOW.1, CELL_H, 0.0, (0.0, 0.0))
    }

    fn laid_out(n_panes: usize, n_rows: usize, scroll: f64) -> Layout {
        layout(frame(), 0.0, n_panes, n_rows, scroll, CELL_W, CELL_H)
    }

    fn centre(r: Rect) -> (f64, f64) {
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    }

    #[test]
    fn process_panel_is_the_wide_step_and_as_tall_as_its_rows() {
        let f = frame();
        assert_eq!(f.w, spec::px(metric::PANEL_W));
        assert_eq!((f.x, f.y_top), ((WINDOW.0 - f.w) * 0.5, spec::snap((WINDOW.1 - f.h) * 0.5)));
        assert_eq!((f.x.fract(), f.y_top.fract(), f.h.fract()), (0.0, 0.0, 0.0), "on whole pixels");
        let l = laid_out(4, metric::VISIBLE_ROWS, 0.0);
        assert_eq!(l.processes.rows.len(), metric::VISIBLE_ROWS);
        assert_eq!(l.processes.max_scroll, 0.0);
        // A window too small for it gets what the window allows.
        let small = panel_rect(900.0, 400.0, CELL_H, 0.0, (0.0, 0.0));
        assert_eq!(small.w, 900.0 * spec::panel::MAX_W_FRAC);
        assert_eq!(small.h, 400.0 * spec::panel::MAX_H_FRAC);
    }

    /// The title used to be drawn above the frame.
    #[test]
    fn process_panel_title_baseline_is_inside_the_frame() {
        let l = laid_out(4, 10, 0.0);
        let (f, c) = (l.chrome.frame, l.chrome);
        let baseline = c.title_baseline();
        let cap = spec::px(header::TITLE.cap());
        assert!(baseline - cap > f.y_top, "the title's top {} is above the frame's {}", baseline - cap, f.y_top);
        assert!(baseline < c.header.y_top + c.header.h, "the title sits in the header");
        assert!(c.title_x() > f.x && c.title_x() < f.x + f.w);
        // Wherever the panel is dragged, too.
        let moved = panel_rect(WINDOW.0, WINDOW.1, CELL_H, 0.0, (-300.0, 120.0));
        let c = PanelChrome::new(moved, 0.0);
        assert!(c.title_baseline() - cap > moved.y_top);
        assert!(c.title_baseline() < moved.y_top + moved.h);
    }

    #[test]
    fn process_panel_close_hit_box_is_where_the_chrome_draws_it() {
        let l = laid_out(4, 10, 0.0);
        let close = PanelChrome::new(frame(), 0.0).close;
        assert_eq!(
            (l.chrome.close.x, l.chrome.close.y_top, l.chrome.close.w, l.chrome.close.h),
            (close.x, close.y_top, close.w, close.h),
        );
        let (cx, cy) = centre(close);
        assert_eq!(l.hover_at(cx, cy), Hover::Close);
        assert_eq!(l.hover_at(close.x + 0.5, close.y_top + 0.5), Hover::Close);
        assert_eq!(l.hover_at(close.x - 1.0, cy), Hover::None);
        assert_eq!(l.hover_at(cx, close.y_top + close.h + 1.0), Hover::None);
        assert!(!l.hit_header(cx, cy), "the close button is not a drag handle");
        assert!(l.hit_header(l.chrome.title_x(), cy));
    }

    #[test]
    fn process_panel_kill_hit_box_is_the_rect_it_is_painted_in() {
        let l = laid_out(4, 10, 0.0);
        assert_eq!(l.kills.len(), 10, "every laid-out row has a button");
        let (side, _) = controls::danger_button_size_in_row(table::row_height(CELL_H));
        for &(i, k) in &l.kills {
            let row = l.processes.rows.iter().find(|(r, _)| *r == i).unwrap().1;
            assert_eq!((k.w, k.h), (side, side));
            assert!(k.y_top > row.y_top && k.y_top + k.h < row.y_top + row.h, "inside its row");
            assert!((k.y_top - row.y_top - (row.y_top + row.h - k.y_top - k.h)).abs() < 1e-9, "centred");
            let right = l.processes.card.x + l.processes.card.w - spec::px(card::ROW_PAD_X);
            assert!((k.x + k.w - right).abs() < 1e-9, "at the right end of the row");
            // Every corner of the rect is the button; a pixel outside
            // it is the row.
            for (x, y) in [
                (k.x, k.y_top),
                (k.x + k.w - 0.01, k.y_top),
                (k.x, k.y_top + k.h - 0.01),
                (k.x + k.w - 0.01, k.y_top + k.h - 0.01),
            ] {
                assert_eq!(l.hover_at(x, y), Hover::Kill(i));
            }
            assert_eq!(l.hover_at(k.x - 0.5, k.y_top), Hover::Process(i));
            assert_eq!(l.hover_at(k.x, k.y_top - 0.5), Hover::Process(i));
            assert_eq!(l.hover_at(k.x, k.y_top + k.h + 0.01), Hover::Process(i));
        }
    }

    #[test]
    fn process_panel_long_name_is_cut_before_the_pid_column() {
        let l = laid_out(4, 10, 0.0);
        let g = &l.processes;
        let row = g.rows[0].1;
        let long = "a-command-name-that-goes-on-".repeat(12);
        let (rect, text) = g.cell_text(row, col::NAME, 4, &long);
        assert!(text.chars().count() < long.chars().count());
        assert!(text.ends_with('…'));
        let end = rect.x + text.chars().count() as f64 * CELL_W;
        let pid_x = g.columns[col::PID].0;
        assert!(
            end <= pid_x - t::COLUMN_GAP_CELLS * CELL_W + 1e-9,
            "the name ends at {end}, the pid column starts at {pid_x}",
        );
        // A name that fits is left alone.
        assert_eq!(g.cell_text(row, col::NAME, 4, "rust-analyzer-p").1, "rust-analyzer-p");
    }

    #[test]
    fn process_panel_rows_beyond_the_card_are_not_laid_out() {
        let n = metric::VISIBLE_ROWS + 30;
        let l = laid_out(4, n, 0.0);
        let g = &l.processes;
        assert_eq!(g.rows.len(), metric::VISIBLE_ROWS);
        assert_eq!(l.kills.len(), g.rows.len(), "no button without its row");
        let bottom = g.card.y_top + g.card.h;
        for &(_, r) in &g.rows {
            assert!(r.y_top >= g.header.y_top + g.header.h && r.y_top + r.h <= bottom);
        }
        // Below the card there is nothing to click.
        assert_eq!(l.hover_at(g.card.x + 20.0, bottom + 5.0), Hover::None);
        // Scrolled to the end, the last row is the one at the bottom
        // and the first ones are gone.
        let (_, max) = l.scroll();
        assert!((max - 30.0 * table::row_height(CELL_H)).abs() < 1.0);
        let l = laid_out(4, n, max + 500.0);
        assert_eq!(l.scroll(), (max, max));
        assert_eq!(l.processes.rows.first().unwrap().0, 30);
        assert_eq!(l.processes.rows.last().unwrap().0, n - 1);
        // More panes than fit are left out the same way.
        let l = laid_out(metric::VISIBLE_ROWS + 5, 3, 0.0);
        assert_eq!(l.panes.rows.len(), metric::VISIBLE_ROWS);
    }

    #[test]
    fn process_panel_cards_share_a_top_and_a_height_and_fill_the_content() {
        let l = laid_out(4, 10, 0.0);
        let (a, b, c) = (l.panes.card, l.processes.card, l.chrome.content);
        assert_eq!((a.y_top, a.h), (b.y_top, b.h));
        assert_eq!(a.x, c.x);
        assert!((b.x - (a.x + a.w) - spec::px(space::CARD_TO_CARD)).abs() < 1e-9);
        assert!((b.x + b.w - (c.x + c.w)).abs() < 1e-9);
        assert!((a.y_top + a.h - (c.y_top + c.h)).abs() < 1e-9);
        assert_eq!(a.y_top, spec::snap(c.y_top + panel_chrome::heading_height()));
        assert_eq!((l.panes_heading.x, l.panes_heading.w), (a.x, a.w));
        assert_eq!((l.processes_heading.x, l.processes_heading.w), (b.x, b.w));
        assert_eq!(c.x - l.chrome.frame.x, spec::px(content::PAD_X));
        // The left card shows a whole pane name at this font.
        assert!((l.panes.columns[col::NAME].1 - metric::PANE_NAME_CELLS * CELL_W).abs() < 1e-9);
    }

    #[test]
    fn process_panel_rows_select_and_the_pointer_finds_them() {
        let l = laid_out(4, 10, 0.0);
        for &(i, r) in &l.panes.rows {
            assert_eq!(l.hover_at(r.x + 30.0, r.y_top + r.h * 0.5), Hover::Pane(i));
        }
        let (_, r) = l.processes.rows[3];
        assert_eq!(l.hover_at(r.x + 30.0, r.y_top + 1.0), Hover::Process(3));
        // The headings, the table headers and the gap between the
        // cards are nothing.
        let (hx, hy) = centre(l.panes.header);
        assert_eq!(l.hover_at(hx, hy), Hover::None);
        assert_eq!(l.hover_at(l.panes_heading.x + 4.0, l.panes_heading.top + 4.0), Hover::None);
        let between = l.panes.card.x + l.panes.card.w + 2.0;
        assert_eq!(l.hover_at(between, r.y_top + 1.0), Hover::None);
        assert!(l.contains(between, r.y_top + 1.0));
        assert!(!l.contains(l.chrome.frame.x - 1.0, r.y_top));
    }

    #[test]
    fn process_panel_dragged_keeps_its_header_reachable() {
        let top_inset = 60.0;
        let at = |dx: f64, dy: f64| panel_rect(WINDOW.0, WINDOW.1, CELL_H, top_inset, (dx, dy));
        let home = at(0.0, 0.0);
        let moved = at(50.0, 30.0);
        assert_eq!((moved.x - home.x, moved.y_top - home.y_top), (50.0, 30.0));
        assert_eq!((moved.w, moved.h), (home.w, home.h));
        assert_eq!(at(0.0, -1e6).y_top, top_inset, "never under the window's own chrome");
        assert_eq!(at(0.0, 1e6).y_top, WINDOW.1 - spec::px(header::H));
        assert_eq!(at(-1e6, 0.0).x, 0.0);
        assert_eq!(at(1e6, 0.0).x, WINDOW.0 - home.w);
    }

    #[test]
    fn process_panel_formats_memory_by_magnitude() {
        assert_eq!(format_rss(512), "512K");
        assert_eq!(format_rss(86 * 1024), "86M");
        assert_eq!(format_rss(1024 * 1024 + 512 * 1024), "1.5G");
    }
}
