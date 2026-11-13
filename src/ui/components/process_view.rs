//! The process monitor as a tree: the Panes table, the selected
//! pane's process table, and a kill button at the end of each of its
//! rows.
//!
//! The tables are tabular data on the terminal font's cell, and where
//! each card, column and row is comes from `process_panel::layout`.
//! Every cell is a text node that stands on the rectangle that layout
//! gives it and carries its text already cut to its column; a row's
//! fill lights with the row and with the button at its end.  The panes
//! are one list, moved through with the arrows.  A kill button answers
//! a press and nothing else.

use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

use crate::ui::components::controls::Measure;
use crate::ui::components::process_panel::{
    self as pp, col, pane_columns, process_columns, Hover, Layout, ProcessPanelPaneRow, ProcessPanelRow,
};
use crate::ui::components::table::{Align as ColAlign, Column, SortDir, TableGeometry};
use crate::ui::components::panel_chrome;
use crate::ui::core::ViewPainter;
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, card, content, control, space, table as ts};
use crate::ui::theme::PanelText;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{FontMeasure, KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{Action, Align, AlignCross, Edges, GroupKind, Length, TextFont, Ui, WidgetId};
use marspot_term::layout::Rect;

/// Below this a process counts as idle: it would print as `0.0`.
const IDLE_CPU_PCT: f32 = 0.05;

/// A pane was chosen: the argument is its row.
pub const CHOOSE_PANE: u32 = 5;
/// A process row was pressed, which does nothing.
pub const ON_PROCESS: u32 = 6;
/// A kill button was pressed: the argument is its row.
pub const KILL_ROW: u32 = 7;

const PANES_ID: WidgetId = WidgetId(1);
const PROCESS_ID: u32 = 10_000;
const KILL_ID: u32 = 20_000;
/// What a row of each table lights with.
const PANE_LIT: u32 = 1;
const PROCESS_LIT: u32 = 10_001;

/// What an action of this panel asks for, as the panel names it.
pub fn asked(action: Action) -> Hover {
    let row = action.arg as usize;
    match action.id {
        CHOOSE_PANE => Hover::Pane(row),
        ON_PROCESS => Hover::Process(row),
        KILL_ROW => Hover::Kill(row),
        _ if action == panel::CLOSE => Hover::Close,
        _ => Hover::None,
    }
}

/// Everything one frame of the panel is drawn from.
#[derive(Clone)]
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
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: Rc<RefCell<ProcessSurface>>,
}

/// The corner everything is placed from, and the string a value is
/// written into before it is cut to its column.
struct Sheet<'s> {
    origin: Rect,
    scratch: &'s mut String,
}

impl Sheet<'_> {
    /// `f`'s node with its top-left corner at `at` in the window.
    fn at(&self, ui: &mut Ui<'_>, at: (f64, f64), f: impl FnOnce(&mut Ui<'_>)) {
        let (left, top) = ((at.0 - self.origin.x).max(0.0), (at.1 - self.origin.y_top).max(0.0));
        let none = Length::ZERO;
        ui.vstack(f).padding(Edges { top: Length::Px(top), right: none, bottom: none, left: Length::Px(left) });
    }

    /// `text` in column `col` of `row`, `indent` tree levels in, cut
    /// to what the column holds.
    #[allow(clippy::too_many_arguments)]
    fn cell(&self, ui: &mut Ui<'_>, g: &TableGeometry, row: Rect, col: usize, align: ColAlign, indent: u8, text: &str, ink: Tone) {
        let (rect, len, cut) = g.cell(row, col, indent, text);
        let anchor = match align {
            ColAlign::Left => Align::Leading,
            ColAlign::Right => Align::Trailing,
        };
        self.at(ui, (rect.x, rect.y_top), |ui| {
            ui.text_fmt(TextFont::Mono, ink.into(), format_args!("{}{}", &text[..len], if cut { "…" } else { "" }))
                .width(Length::Px(rect.w))
                .height(Length::Px(rect.h))
                .align(anchor);
        });
    }

    /// [`Self::cell`] for the value last written into the scratch.
    fn value(&self, ui: &mut Ui<'_>, g: &TableGeometry, row: Rect, col: usize, ink: Tone) {
        self.cell(ui, g, row, col, ColAlign::Right, 0, self.scratch, ink);
    }

    /// A table's card, the labels of its header and the line under
    /// them.
    fn frame(&mut self, ui: &mut Ui<'_>, g: &TableGeometry, columns: &[Column]) {
        self.at(ui, (g.card.x, g.card.y_top), |ui| {
            ui.leaf(0, Length::Px(g.card.w), Length::Px(g.card.h))
                .fill(Tone::CardBg.into())
                .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
                .radius(Length::Pt(card::RADIUS));
        });
        for (col, c) in columns.iter().enumerate() {
            // The arrow goes on the side away from the edge the column
            // is aligned to, so the label still lines up with its data.
            self.scratch.clear();
            let arrow = c.sort.map(|dir| if dir == SortDir::Asc { '▲' } else { '▼' });
            let _ = match (arrow, c.align) {
                (None, _) => write!(self.scratch, "{}", c.header),
                (Some(a), ColAlign::Left) => write!(self.scratch, "{} {a}", c.header),
                (Some(a), ColAlign::Right) => write!(self.scratch, "{a} {}", c.header),
            };
            if !self.scratch.is_empty() {
                self.cell(ui, g, g.header, col, c.align, 0, self.scratch, Tone::FgMuted);
            }
        }
        let line = panel_chrome::row_separator_rect(g.card, g.separator_y);
        self.at(ui, (line.x, line.y_top), |ui| {
            ui.leaf(0, Length::Px(line.w), Length::Px(line.h)).fill(Tone::Separator.into());
        });
    }

    /// The fill behind `row`: short of the card's edges, there when
    /// the row is `selected` or the pointer is on anything of `group`.
    fn highlight(&self, ui: &mut Ui<'_>, g: &TableGeometry, row: Rect, selected: bool, group: u32) {
        let r = g.highlight_rect(row);
        self.at(ui, (r.x, r.y_top), |ui| {
            let node = ui
                .leaf(0, Length::Px(r.w), Length::Px(r.h))
                .radius(Length::Pt(ts::SELECTED_RADIUS))
                .lit_with(group);
            if selected {
                node.fill(Tone::RowSelected.into());
            } else {
                node.fill_hover(Tone::RowHover.into());
            }
        });
    }
}

/// Resident memory for the RSS column, written into `out`.
fn write_rss(out: &mut String, kb: u64) {
    const K: u64 = 1024;
    let _ = if kb >= K * K {
        write!(out, "{:.1}G", kb as f64 / (K * K) as f64)
    } else if kb >= K {
        write!(out, "{:.0}M", kb as f64 / K as f64)
    } else {
        write!(out, "{kb}K")
    };
}

/// `text` cut, with an ellipsis, to what a heading `w` wide has left
/// beside its title; empty when not even the ellipsis fits.
fn fit_trailing(text: &str, w: f64, m: Measure<'_>, out: &mut String) {
    fn width(m: Measure<'_>, role: PanelText, s: &str) -> f64 {
        m(s, role.pt(), role.weight()) as f32 as f64
    }
    let room = w - width(m, content::HEADING, t(pp::PROCESSES_HEADING)) - spec::px(space::S12);
    out.clear();
    out.push_str(text);
    if width(m, content::HEADING_TRAILING, out) <= room {
        return;
    }
    // from the longest beginning down to none of it
    let mut end = text.len();
    while let Some((at, _)) = text[..end].char_indices().next_back() {
        end = at;
        out.clear();
        out.push_str(&text[..end]);
        out.push('…');
        if width(m, content::HEADING_TRAILING, out) <= room {
            return;
        }
    }
    out.clear();
}

/// The panel for `panel`: a node on every rectangle its layout has.
pub fn describe(ui: &mut Ui<'_>, fm: &FontMeasure<'_>, panel: &ProcessPanelRender, scratch: &mut String) {
    let l = &panel.layout;
    let frame = l.chrome.frame;
    let mut s = Sheet { origin: l.chrome.content, scratch };
    let heading_h = Length::Pt(content::HEADING.cap());
    panel::panel(ui, (frame.w, frame.h), l.chrome.backdrop_top, t(pp::TITLE), None, |ui| {
        ui.zstack(Align::TopLeading, |ui| {
            // the left group: one row per pane
            let h = l.panes_heading;
            s.at(ui, (h.x, h.top), |ui| {
                ui.text(font_of(content::HEADING), Tone::Fg.into(), t(pp::PANES_HEADING));
            });
            let g = &l.panes;
            s.frame(ui, g, &pane_columns());
            for &(i, row) in &g.rows {
                let Some(pane) = panel.pane_rows.get(i) else { continue };
                let selected = i == panel.selected_pane;
                s.highlight(ui, g, row, selected, PANE_LIT + i as u32);
                let number = if selected { Tone::Fg } else { Tone::FgSecondary };
                s.cell(ui, g, row, col::NAME, ColAlign::Left, 0, &pane.name, Tone::Fg);
                s.scratch.clear();
                let _ = write!(s.scratch, "{}", pane.n_pids);
                s.value(ui, g, row, col::PANE_PIDS, number);
                s.scratch.clear();
                let _ = write!(s.scratch, "{:.1}", pane.cpu_pct);
                s.value(ui, g, row, col::PANE_CPU, number);
                s.scratch.clear();
                write_rss(s.scratch, pane.rss_kb);
                s.value(ui, g, row, col::PANE_RSS, number);
            }
            // over the cells, the rows as what the pointer and the
            // arrow keys choose
            if let Some(&(_, first)) = g.rows.first() {
                let step = g.rows.get(1).map_or(first.h, |(_, next)| next.y_top - first.y_top);
                s.at(ui, (first.x, first.y_top), |ui| {
                    ui.group(PANES_ID, GroupKind::List, true, |ui| {
                        for &(i, row) in &g.rows {
                            ui.leaf(0, Length::Px(row.w), Length::Px(row.h))
                                .lit_with(PANE_LIT + i as u32)
                                .item(Action::new(CHOOSE_PANE, i as u32))
                                .current(i == panel.selected_pane);
                        }
                    })
                    .gap(Length::Px((step - first.h).max(0.0)));
                });
            }

            // the right group: the selected pane's process tree
            let h = l.processes_heading;
            fit_trailing(&panel.detail, h.w, &mut fm.for_controls(), s.scratch);
            s.at(ui, (h.x, h.top), |ui| {
                ui.hstack(|ui| {
                    ui.text(font_of(content::HEADING), Tone::Fg.into(), t(pp::PROCESSES_HEADING));
                    ui.spacer();
                    if !s.scratch.is_empty() {
                        ui.text_copy(font_of(content::HEADING_TRAILING), Tone::FgMuted.into(), s.scratch);
                    }
                })
                .width(Length::Px(h.w))
                .height(heading_h)
                .align_cross(AlignCross::End);
            });
            let g = &l.processes;
            // Only the headers are read here; the kill column has
            // none, and its width is already in the layout.
            s.frame(ui, g, &process_columns(0.0));
            for &(i, row) in &g.rows {
                let Some(process) = panel.rows.get(i) else { continue };
                let lit = PROCESS_LIT + i as u32;
                s.highlight(ui, g, row, false, lit);
                s.cell(ui, g, row, col::NAME, ColAlign::Left, process.depth, &process.comm, Tone::Fg);
                s.scratch.clear();
                let _ = write!(s.scratch, "{}", process.pid);
                s.value(ui, g, row, col::PID, Tone::FgSecondary);
                // An idle process shows a dot, so the busy ones stand
                // out of the column.
                if process.cpu_pct > IDLE_CPU_PCT {
                    s.scratch.clear();
                    let _ = write!(s.scratch, "{:.1}", process.cpu_pct);
                    s.value(ui, g, row, col::CPU, Tone::FgSecondary);
                } else {
                    s.cell(ui, g, row, col::CPU, ColAlign::Right, 0, "·", Tone::FgMuted);
                }
                s.scratch.clear();
                write_rss(s.scratch, process.rss_kb);
                s.value(ui, g, row, col::RSS, Tone::FgSecondary);
                s.at(ui, (row.x, row.y_top), |ui| {
                    ui.leaf(0, Length::Px(row.w), Length::Px(row.h))
                        .lit_with(lit)
                        .pressable(WidgetId(PROCESS_ID + i as u32), Action::new(ON_PROCESS, i as u32));
                });
            }
            for &(i, rect) in &l.kills {
                if i < panel.rows.len() {
                    s.at(ui, (rect.x, rect.y_top), |ui| {
                        let k = rect.w / spec::px(control::DANGER_SIZE);
                        ui.leaf(preset::DANGER, Length::Px(rect.w), Length::Px(rect.h))
                            .radius(Length::Pt(control::DANGER_RADIUS * k))
                            .lit_with(PROCESS_LIT + i as u32)
                            // by a press only: ending a process is not
                            // something the Tab key arrives at
                            .pressable(WidgetId(KILL_ID + i as u32), Action::new(KILL_ROW, i as u32));
                    });
                }
            }
        })
        .width(Length::Pct(1.0))
        .grow(1);
    });
}

/// What the panel was last described from.
struct Described {
    frame: Rect,
    backdrop_top: f64,
    /// How far the process list is pushed up, and the first row of
    /// each table: the cell they are set on is in them.
    geometry: (f64, Option<Rect>, Option<Rect>),
    selected_pane: usize,
    detail: String,
    pane_rows: Vec<ProcessPanelPaneRow>,
    rows: Vec<ProcessPanelRow>,
}

/// The process monitor's surface.
#[derive(Default)]
pub struct ProcessSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
    scratch: String,
}

impl ProcessSurface {
    /// Add the panel to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, panel: &ProcessPanelRender) {
        let l = &panel.layout;
        let frame = l.chrome.frame;
        let first = |rows: &[(usize, Rect)]| rows.first().map(|(_, r)| *r);
        let geometry = (l.processes.scroll, first(&l.panes.rows), first(&l.processes.rows));
        let same = self.described.as_ref().is_some_and(|d| {
            d.frame == frame
                && d.backdrop_top == l.chrome.backdrop_top
                && d.geometry == geometry
                && d.selected_pane == panel.selected_pane
                && d.detail == panel.detail
                && d.pane_rows == panel.pane_rows
                && d.rows == panel.rows
        });
        if !same {
            // the copy kept from last time is refreshed where it is
            let d = self.described.get_or_insert_with(|| Described {
                frame,
                backdrop_top: 0.0,
                geometry,
                selected_pane: 0,
                detail: String::new(),
                pane_rows: Vec::new(),
                rows: Vec::new(),
            });
            (d.frame, d.backdrop_top, d.geometry) = (frame, l.chrome.backdrop_top, geometry);
            d.selected_pane = panel.selected_pane;
            d.detail.clone_from(&panel.detail);
            d.pane_rows.clone_from(&panel.pane_rows);
            d.rows.clone_from(&panel.rows);
            self.data_rev += 1;
        }
        let at = Place { data_rev: self.data_rev, origin: (frame.x, frame.y_top), avail: (frame.w, frame.h) };
        let scratch = &mut self.scratch;
        self.kept.frame(p, at, panel.pointer, |ui, fm| describe(ui, fm, panel, scratch));
    }
}
