//! The sidebar as a tree: the add button, then per pane a row and its
//! close button, each a floating node where the layout puts it.
//!
//! Each of them answers a press itself, tested against the kept
//! layout.  The tree keeps what the sidebar last painted: a row, the
//! pane in use or the window changing describes it again, and the
//! pointer moving from one control to another only paints it again.

use std::hash::{Hash, Hasher};

use crate::font_cache::FontCache;
use crate::layout::Layout;
use crate::render::SidebarEntry;
use crate::ui::components::sidebar::{self, Sidebar};
use crate::ui::core::ViewPainter;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use crate::ui::SESSION_COUNT_HARD_CAP;
use golia_ui_core::{Action, Length, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;
use marspot_term::session::SessionState;

/// `data[0]` of a row or its close button: it is the pane in use.
const SELECTED: u32 = 1;
/// `data[0]` of anything: the pointer is not to light it, because a
/// panel or a menu lies over the window.
const QUIET: u32 = 2;
/// `data[0]` of the add button: no more panes can be added.
const FULL: u32 = 4;
/// `data[0]` of a close button: its pane is not the last.
const CAN_CLOSE: u32 = 8;
/// `data[0]` of a row, from this bit up: its session's state.
const STATE_SHIFT: u32 = 4;

/// The actions: the add button's, and row `n`'s or its close button's
/// with `arg: n`.
pub const ADD: u32 = 2;
pub const ROW: u32 = 3;
pub const CLOSE: u32 = 4;

const ADD_ID: WidgetId = WidgetId(0);

fn row_id(i: usize) -> WidgetId {
    WidgetId(1 + 2 * i as u32)
}

fn close_id(i: usize) -> WidgetId {
    WidgetId(2 + 2 * i as u32)
}

/// The rows a sidebar lists.
pub trait Rows {
    fn count(&self) -> usize;
    fn label(&self, i: usize) -> &str;
    fn state(&self, i: usize) -> SessionState;
}

impl Rows for &[SidebarEntry<'_>] {
    fn count(&self) -> usize {
        <[SidebarEntry<'_>]>::len(self)
    }
    fn label(&self, i: usize) -> &str {
        self[i].label
    }
    fn state(&self, i: usize) -> SessionState {
        self[i].state
    }
}

/// `n` rows with nothing to show, for laying the sidebar out where
/// only where its rows are matters.
pub struct Blank(pub usize);

impl Rows for Blank {
    fn count(&self) -> usize {
        self.0
    }
    fn label(&self, _: usize) -> &str {
        ""
    }
    fn state(&self, _: usize) -> SessionState {
        SessionState::Active
    }
}

/// What the sidebar shows this frame.
pub struct SidebarFrame<'a> {
    pub layout: &'a Layout,
    pub rows: &'a dyn Rows,
    pub focused_idx: usize,
    /// A panel or a menu lies over the window: nothing lights under
    /// the pointer, though what the pointer is on is still followed.
    pub quiet: bool,
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
    /// states, where the layout puts them, which is in use.
    fn fingerprint(&self) -> u64 {
        let mut h = marspot_term::fast_hash::FxHasher::default();
        for i in 0..self.rows.count() {
            self.rows.label(i).hash(&mut h);
            state_bits(self.rows.state(i)).hash(&mut h);
        }
        self.focused_idx.hash(&mut h);
        (self.quiet, self.cell_w.to_bits()).hash(&mut h);
        let (measures, add) = self.hit_shape();
        measures.map(f64::to_bits).hash(&mut h);
        rect_bits(&add).hash(&mut h);
        for r in &self.layout.close_session_rects {
            rect_bits(r).hash(&mut h);
        }
        h.finish()
    }

    /// What a press and the pointer are tested against, beside the
    /// close buttons: the layout's measures, the row count and the add
    /// button.
    fn hit_shape(&self) -> ([f64; 6], Rect) {
        let l = self.layout;
        let m = [l.sidebar_w, l.top_inset, l.window_h, l.sidebar_top_pad_phys, l.window_w, self.rows.count() as f64];
        (m, l.add_session_button_rect)
    }
}

/// Where the sidebar's controls were when it was last described.
#[derive(Default)]
struct HitShape {
    measures: Option<([f64; 6], Rect)>,
    closes: Vec<Rect>,
}

impl HitShape {
    fn is(&self, f: &SidebarFrame<'_>) -> bool {
        self.measures == Some(f.hit_shape()) && self.closes == f.layout.close_session_rects
    }

    fn keep(&mut self, f: &SidebarFrame<'_>) {
        self.measures = Some(f.hit_shape());
        self.closes.clear();
        self.closes.extend_from_slice(&f.layout.close_session_rects);
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
    let quiet = if f.quiet { QUIET } else { 0 };
    ui.vstack(|ui| {
        if l.sidebar_w <= 0.0 {
            return;
        }
        let add = l.add_session_button_rect;
        if add.w > 0.0 {
            let full = l.close_session_rects.len() >= SESSION_COUNT_HARD_CAP;
            let state = if full { FULL } else { 0 } | quiet;
            at(ui, add, |ui| {
                ui.leaf(preset::SIDEBAR_ADD, Length::Px(add.w), Length::Px(add.h))
                    .leaf(preset::SIDEBAR_ADD, [state, (l.sidebar_w as f32).to_bits()])
                    .pressable(ADD_ID, Action::new(ADD, 0));
            });
        }
        let column = f.column();
        // the last pane cannot be closed
        let can_close = if f.rows.count() > 1 { CAN_CLOSE } else { 0 };
        for i in 0..f.rows.count() {
            let rect = column.row_rect(i);
            let selected = if i == f.focused_idx { SELECTED } else { 0 };
            let close = l.close_session_rects.get(i);
            let name = sidebar::clip_chars(f.rows.label(i), sidebar::name_room(rect, close, f.cell_w));
            let state = selected | quiet | state_bits(f.rows.state(i)) << STATE_SHIFT;
            // a row and its close button light together
            let group = i as u32 + 1;
            at(ui, rect, |ui| {
                ui.leaf(preset::SIDEBAR_ROW, Length::Px(rect.w), Length::Px(rect.h))
                    .leaf(preset::SIDEBAR_ROW, [state, 0])
                    .label_copy(name)
                    .pressable(row_id(i), Action::new(ROW, i as u32))
                    .lit_with(group);
            });
            // there whether it shows or not: it shows on the pane in use
            // and under the pointer, and takes a press wherever it is
            if let Some(&close) = close {
                at(ui, close, |ui| {
                    ui.leaf(preset::SIDEBAR_CLOSE, Length::Px(close.w), Length::Px(close.h))
                        .leaf(preset::SIDEBAR_CLOSE, [selected | quiet | can_close, 0])
                        .pressable(close_id(i), Action::new(CLOSE, i as u32))
                        .lit_with(group);
                });
            }
        }
    });
}

/// Whether the pointer lights a control with this state and these bits.
fn lit(data: u32, state: WidgetState) -> bool {
    data & QUIET == 0 && (state.hovered || state.lit)
}

pub(crate) fn paint_add(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2], state: WidgetState) {
    let sidebar_w = f32::from_bits(data[1]) as f64;
    sidebar::paint_add(p, rect, sidebar_w, data[0] & FULL != 0, lit(data[0], state));
}

pub(crate) fn paint_row(p: &mut ViewPainter<'_>, rect: Rect, name: &str, data: [u32; 2], state: WidgetState) {
    let session = state_of(data[0] >> STATE_SHIFT);
    sidebar::paint_row(p, rect, name, session, data[0] & SELECTED != 0, lit(data[0], state));
}

pub(crate) fn paint_close(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2], state: WidgetState) {
    if data[0] & SELECTED == 0 && !lit(data[0], state) {
        return;
    }
    let hot = data[0] & QUIET == 0 && state.hovered;
    sidebar::paint_close(p, rect, sidebar::close_look(data[0] & CAN_CLOSE != 0, hot));
}

/// The sidebar's surface: its tree, its layout and the instances it
/// last painted.
#[derive(Default)]
pub struct SidebarSurface {
    pub kept: KeptSurface,
    described: Option<(u64, (f64, f64))>,
    hit: HitShape,
    data_rev: u64,
}

impl SidebarSurface {
    fn place(&mut self, f: &SidebarFrame<'_>, window: (f64, f64)) -> Place {
        let now = Some((f.fingerprint(), window));
        if self.described != now {
            self.described = now;
            self.hit.keep(f);
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Add the sidebar to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, f: &SidebarFrame<'_>) {
        let at = self.place(f, (p.window_w, p.window_h));
        self.kept.frame(p, at, None, |ui, _| describe(ui, f));
    }

    /// Lay the sidebar of a window laid out as `layout`, listing
    /// `rows` panes, out again if its rows or buttons have moved since
    /// it was last described, so input that arrives before the next
    /// frame is tested against where that frame will draw them.  The
    /// rows are laid out blank: that frame describes them again.
    pub fn ready(&mut self, font: &mut FontCache, layout: &Layout, rows: usize) {
        let f = SidebarFrame { layout, rows: &Blank(rows), focused_idx: 0, quiet: false, cell_w: font.cell_w as f32 };
        if self.hit.is(&f) {
            return;
        }
        let at = self.place(&f, (layout.window_w, layout.window_h));
        self.kept.lay_out(font, at, |ui, _| describe(ui, &f));
    }
}
