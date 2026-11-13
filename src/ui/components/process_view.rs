//! The process monitor as a tree: the Panes list, the selected pane's
//! process rows, and a kill button at the end of each.
//!
//! The two tables are tabular data on the terminal font's cell, placed
//! by `process_panel::layout`; each row and each button is a node that
//! stands on the rectangle that layout gives it and is drawn by
//! `process_panel_paint` there.  The tree keeps the picture and takes
//! the pointer and the keyboard: the panes are one list, moved through
//! with the arrows.  A kill button answers a press and nothing else.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ui::components::controls;
use crate::ui::components::process_panel::{Hover, Layout, ProcessPanelPaneRow, ProcessPanelRow};
use crate::ui::components::process_panel_paint as paint;
use crate::ui::core::ViewPainter;
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, content, control};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, panel, preset};
use golia_ui_core::{Action, Align, Edges, GroupKind, Length, Target, Ui, WidgetId};
use marspot_term::layout::Rect;

/// The Panes heading and its table's frame.
const PANES: u32 = preset::CUSTOM + 10;
/// One row of the Panes table.  `data[0]` is the pane.
const PANE_ROW: u32 = preset::CUSTOM + 11;
/// The Processes heading and its table's frame.
const PROCESSES: u32 = preset::CUSTOM + 12;
/// One row of the Processes table.  `data[0]` is the row.
const PROCESS_ROW: u32 = preset::CUSTOM + 13;
/// A row's kill button.  `data[0]` is the row.
const KILL: u32 = preset::CUSTOM + 14;

/// A pane was chosen: the argument is its row.
pub const CHOOSE_PANE: u32 = 5;
/// A process row was pressed, which does nothing.
pub const ON_PROCESS: u32 = 6;
/// A kill button was pressed: the argument is its row.
pub const KILL_ROW: u32 = 7;

const PANES_ID: WidgetId = WidgetId(1);
const PROCESS_ID: u32 = 10_000;
const KILL_ID: u32 = 20_000;

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

/// `f`'s node with its top-left corner `at` from the corner of the
/// enclosing stack of overlaid nodes.
fn placed(ui: &mut Ui<'_>, at: (f64, f64), f: impl FnOnce(&mut Ui<'_>)) {
    let none = Length::ZERO;
    ui.vstack(f).padding(Edges { top: Length::Px(at.1.max(0.0)), right: none, bottom: none, left: Length::Px(at.0.max(0.0)) });
}

/// The panel for `panel`: a node on every rectangle its layout has.
pub fn describe(ui: &mut Ui<'_>, panel: &ProcessPanelRender) {
    let l = &panel.layout;
    let frame = l.chrome.frame;
    let o = l.chrome.content;
    let from = |r: Rect| (r.x - o.x, r.y_top - o.y_top);
    let size = |r: Rect| (Length::Px(r.w), Length::Px(r.h));
    let heading_h = spec::px(content::HEADING.cap());
    panel::panel(ui, (frame.w, frame.h), l.chrome.backdrop_top, t(super::process_panel::TITLE), None, |ui| {
        ui.zstack(Align::TopLeading, |ui| {
            let h = l.panes_heading;
            placed(ui, (h.x - o.x, h.top - o.y_top), |ui| {
                ui.leaf(PANES, Length::Px(h.w), Length::Px(heading_h));
            });
            if let Some(&(_, first)) = l.panes.rows.first() {
                let step = l.panes.rows.get(1).map_or(first.h, |(_, next)| next.y_top - first.y_top);
                placed(ui, from(first), |ui| {
                    ui.group(PANES_ID, GroupKind::List, true, |ui| {
                        for &(i, row) in &l.panes.rows {
                            let (w, h) = size(row);
                            ui.leaf(PANE_ROW, w, h)
                                .leaf(PANE_ROW, [i as u32, 0])
                                .item(Action::new(CHOOSE_PANE, i as u32))
                                .current(i == panel.selected_pane);
                        }
                    })
                    .gap(Length::Px((step - first.h).max(0.0)));
                });
            }
            let h = l.processes_heading;
            placed(ui, (h.x - o.x, h.top - o.y_top), |ui| {
                ui.leaf(PROCESSES, Length::Px(h.w), Length::Px(heading_h));
            });
            for &(i, row) in &l.processes.rows {
                if i < panel.rows.len() {
                    placed(ui, from(row), |ui| {
                        let (w, h) = size(row);
                        ui.leaf(PROCESS_ROW, w, h)
                            .leaf(PROCESS_ROW, [i as u32, 0])
                            .pressable(WidgetId(PROCESS_ID + i as u32), Action::new(ON_PROCESS, i as u32));
                    });
                }
            }
            for &(i, rect) in &l.kills {
                if i < panel.rows.len() {
                    placed(ui, from(rect), |ui| {
                        let (w, h) = size(rect);
                        let k = rect.w / spec::px(control::DANGER_SIZE);
                        ui.leaf(KILL, w, h)
                            .leaf(KILL, [i as u32, 0])
                            .radius(Length::Pt(control::DANGER_RADIUS * k))
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
}

impl ProcessSurface {
    /// What the pointer is on, as the panel names it.
    pub fn hovered(&self, panel: &ProcessPanelRender) -> Hover {
        match self.kept.surface.interaction.hovered {
            Some(Target { id, .. }) if id == panel::CLOSE_ID => Hover::Close,
            Some(Target { id, item }) if id == PANES_ID => {
                panel.layout.panes.rows.get(item as usize).map_or(Hover::None, |(i, _)| Hover::Pane(*i))
            }
            Some(Target { id: WidgetId(id), .. }) if id >= KILL_ID => Hover::Kill((id - KILL_ID) as usize),
            Some(Target { id: WidgetId(id), .. }) if id >= PROCESS_ID => Hover::Process((id - PROCESS_ID) as usize),
            _ => Hover::None,
        }
    }

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
            self.described = Some(Described {
                frame,
                backdrop_top: l.chrome.backdrop_top,
                geometry,
                selected_pane: panel.selected_pane,
                detail: panel.detail.clone(),
                pane_rows: panel.pane_rows.clone(),
                rows: panel.rows.clone(),
            });
            self.data_rev += 1;
        }
        let at = Place { data_rev: self.data_rev, origin: (frame.x, frame.y_top), avail: (frame.w, frame.h) };
        // what the pointer is on is found in the layout this frame is
        // painted from, before anything is painted: a row is drawn
        // lit while the pointer is on its kill button too
        self.kept.lay_out(&mut *p.font, at, |ui, _| describe(ui, panel));
        if let Some(pointer) = panel.pointer {
            self.kept.surface.pointer_move(pointer);
        }
        let on = self.hovered(panel);
        let mut inside = |p: &mut ViewPainter<'_>, tag: u32, data: [u32; 2]| {
            let i = data[0] as usize;
            let row_of = |rows: &[(usize, Rect)]| rows.iter().find(|(n, _)| *n == i).map(|(_, r)| *r);
            match tag {
                PANES => paint::paint_panes_frame(p, panel),
                PROCESSES => paint::paint_processes_frame(p, panel),
                PANE_ROW => {
                    if let Some(row) = row_of(&l.panes.rows) {
                        paint::paint_pane_row(p, panel, i, row, on == Hover::Pane(i))
                    }
                }
                PROCESS_ROW => {
                    if let Some(row) = row_of(&l.processes.rows) {
                        let lit = matches!(on, Hover::Process(n) | Hover::Kill(n) if n == i);
                        paint::paint_process_row(p, panel, i, row, lit)
                    }
                }
                KILL => {
                    if let Some(rect) = row_of(&l.kills) {
                        controls::paint_danger_button(p, rect, on == Hover::Kill(i))
                    }
                }
                // no other tag is described
                _ => unreachable!(),
            }
        };
        self.kept.frame_with(p, at, None, |ui, _| describe(ui, panel), &mut inside);
    }
}
