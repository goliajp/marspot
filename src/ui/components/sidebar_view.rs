//! The sidebar as a tree: the add button, then per pane a row and,
//! where it shows, the row's close button, each a floating node where
//! the layout puts it.
//!
//! Where the rows and buttons are stays the layout's, because a press
//! is still tested against it there.  The tree keeps what the sidebar
//! last painted and paints it again only when a row, the pane in use,
//! the pointer's place on it or the window changes.

use std::hash::{Hash, Hasher};

use crate::layout::Layout;
use crate::render::SidebarEntry;
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::sidebar::{self, CloseLook, Sidebar};
use crate::ui::core::ViewPainter;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use crate::ui::SESSION_COUNT_HARD_CAP;
use golia_ui_core::{Length, Ui};
use marspot_term::layout::Rect;
use marspot_term::session::SessionState;

/// `data[0]` of a row: it is the pane in use.
const SELECTED: u32 = 1;
/// `data[0]` of a row or the add button: the pointer is on it.
const LIT: u32 = 2;
/// `data[0]` of the add button: no more panes can be added.
const FULL: u32 = 4;
/// `data[0]` of a row, from this bit up: its session's state.
const STATE_SHIFT: u32 = 3;

/// What the sidebar shows this frame.
pub struct SidebarFrame<'a> {
    pub layout: &'a Layout,
    pub rows: &'a [SidebarEntry<'a>],
    pub focused_idx: usize,
    pub hover: Option<ChromeHover>,
    /// The mono cell's width, which decides how much of a name fits.
    pub cell_w: f32,
}

impl SidebarFrame<'_> {
    fn column(&self) -> Sidebar {
        let l = self.layout;
        Sidebar {
            rect: Rect { x: 0.0, y_top: l.top_inset, w: l.sidebar_w, h: (l.window_h - l.top_inset).max(0.0) },
            top_pad: l.sidebar_top_pad_phys,
        }
    }

    /// Everything the picture depends on, folded: the rows' names and
    /// states, where the layout puts them, which is in use and lit.
    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let l = self.layout;
        for r in self.rows {
            r.label.hash(&mut h);
            state_bits(r.state).hash(&mut h);
        }
        for r in l.close_session_rects.iter().chain([&l.add_session_button_rect]) {
            rect_bits(r).hash(&mut h);
        }
        (l.sidebar_w.to_bits(), l.top_inset.to_bits(), l.window_h.to_bits(), l.sidebar_top_pad_phys.to_bits()).hash(&mut h);
        (self.rows.len(), self.focused_idx, self.hover, self.cell_w.to_bits()).hash(&mut h);
        h.finish()
    }
}

fn rect_bits(r: &Rect) -> [u64; 4] {
    [r.x.to_bits(), r.y_top.to_bits(), r.w.to_bits(), r.h.to_bits()]
}

fn state_bits(s: SessionState) -> u32 {
    match s {
        SessionState::Active => 0,
        SessionState::Idle => 1,
        SessionState::Exited => 2,
    }
}

fn state_of(bits: u32) -> SessionState {
    match bits {
        0 => SessionState::Active,
        1 => SessionState::Idle,
        _ => SessionState::Exited,
    }
}

fn at(ui: &mut Ui<'_>, r: Rect, f: impl FnOnce(&mut Ui<'_>)) {
    ui.float(r.x, r.y_top, f);
}

pub fn describe(ui: &mut Ui<'_>, f: &SidebarFrame<'_>) {
    let l = f.layout;
    ui.vstack(|ui| {
        if l.sidebar_w <= 0.0 {
            return;
        }
        let add = l.add_session_button_rect;
        if add.w > 0.0 {
            let full = l.close_session_rects.len() >= SESSION_COUNT_HARD_CAP;
            let lit = f.hover == Some(ChromeHover::Add);
            let state = if full { FULL } else { 0 } | if lit { LIT } else { 0 };
            at(ui, add, |ui| {
                ui.leaf(preset::SIDEBAR_ADD, Length::Px(add.w), Length::Px(add.h))
                    .leaf(preset::SIDEBAR_ADD, [state, (l.sidebar_w as f32).to_bits()]);
            });
        }
        let column = f.column();
        // the last pane cannot be closed
        let can_close = f.rows.len() > 1;
        for (i, row) in f.rows.iter().enumerate() {
            let rect = column.row_rect(i);
            let selected = i == f.focused_idx;
            let lit = f.hover.and_then(ChromeHover::row) == Some(i);
            let close = l.close_session_rects.get(i);
            let name = sidebar::clip_chars(row.label, sidebar::name_room(rect, close, f.cell_w));
            let state = if selected { SELECTED } else { 0 } | if lit { LIT } else { 0 } | state_bits(row.state) << STATE_SHIFT;
            at(ui, rect, |ui| {
                ui.leaf(preset::SIDEBAR_ROW, Length::Px(rect.w), Length::Px(rect.h))
                    .leaf(preset::SIDEBAR_ROW, [state, 0])
                    .label_copy(name);
            });
            if let Some(&close) = close.filter(|_| selected || lit) {
                let look = sidebar::close_look(can_close, f.hover == Some(ChromeHover::Close(i)));
                at(ui, close, |ui| {
                    ui.leaf(preset::SIDEBAR_CLOSE, Length::Px(close.w), Length::Px(close.h))
                        .leaf(preset::SIDEBAR_CLOSE, [look as u32, 0]);
                });
            }
        }
    });
}

pub(crate) fn paint_add(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2]) {
    let sidebar_w = f32::from_bits(data[1]) as f64;
    sidebar::paint_add(p, rect, sidebar_w, data[0] & FULL != 0, data[0] & LIT != 0);
}

pub(crate) fn paint_row(p: &mut ViewPainter<'_>, rect: Rect, name: &str, data: [u32; 2]) {
    let state = state_of(data[0] >> STATE_SHIFT);
    sidebar::paint_row(p, rect, name, state, data[0] & SELECTED != 0, data[0] & LIT != 0);
}

pub(crate) fn paint_close(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2]) {
    sidebar::paint_close(p, rect, CloseLook::of(data[0]));
}

/// The sidebar's surface: its tree, its layout and the instances it
/// last painted.
#[derive(Default)]
pub struct SidebarSurface {
    pub kept: KeptSurface,
    described: Option<(u64, (f64, f64))>,
    data_rev: u64,
}

impl SidebarSurface {
    fn place(&mut self, f: &SidebarFrame<'_>, window: (f64, f64)) -> Place {
        let now = Some((f.fingerprint(), window));
        if self.described != now {
            self.described = now;
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Add the sidebar to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, f: &SidebarFrame<'_>) {
        let at = self.place(f, (p.window_w, p.window_h));
        self.kept.frame(p, at, None, |ui, _| describe(ui, f));
    }

}
