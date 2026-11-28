//! The sidebar as a tree: the add button where the layout puts it, then
//! one column of rows, each with its close button as the row's spot.
//!
//! Each of them answers a press itself, tested against the kept
//! layout.  The tree keeps what the sidebar last painted: a row, the
//! pane in use or the window changing describes it again, and the
//! pointer moving from one control to another only paints it again.

use std::hash::{Hash, Hasher};

use crate::font_cache::FontCache;
use crate::layout::Layout;
use crate::ui::components::sidebar::{self, Sidebar};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{self, Arg};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use crate::ui::SESSION_COUNT_HARD_CAP;
use golia_ui_core::{Action, Length, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;
use marspot_term::render::SidebarTree;

#[path = "sidebar_bits.rs"]
mod bits;
use bits::*;

/// The actions: the add button's, and row `n`'s or its close button's
/// with `arg: n`.
pub const ADD: u32 = 2;
pub const ROW: u32 = 3;
pub const CLOSE: u32 = 4;
/// The row at the bottom that counts every shadow pane.
pub const HELPERS: u32 = 8;

const ADD_ID: WidgetId = WidgetId(0);
const HELPERS_ID: WidgetId = WidgetId(u32::MAX);

fn row_id(i: usize) -> WidgetId {
    WidgetId(1 + i as u32)
}

#[path = "sidebar_rows.rs"]
mod rows;
pub use rows::{Blank, Rows};

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
    fn fingerprint(&self, helpers: u32) -> u64 {
        let mut h = marspot_term::fast_hash::FxHasher::default();
        for i in 0..self.rows.count() {
            self.rows.label(i).hash(&mut h);
            state_bits(self.rows.state(i)).hash(&mut h);
            self.rows.code(i).hash(&mut h);
            (self.rows.tree(i), self.rows.hollow(i)).hash(&mut h);
        }
        (self.focused_idx, helpers).hash(&mut h);
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
    /// There is a row for the helpers.
    helpers: bool,
}

impl HitShape {
    fn is(&self, f: &SidebarFrame<'_>, helpers: bool) -> bool {
        self.helpers == helpers && self.measures == Some(f.hit_shape()) && self.closes == f.layout.close_session_rects
    }

    fn keep(&mut self, f: &SidebarFrame<'_>, helpers: bool) {
        self.helpers = helpers;
        self.measures = Some(f.hit_shape());
        self.closes.clear();
        self.closes.extend_from_slice(&f.layout.close_session_rects);
    }
}

fn describe(ui: &mut Ui<'_>, f: &SidebarFrame<'_>, helpers: u32) {
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
            ui.float(add.x, add.y_top, |ui| {
                ui.leaf(preset::SIDEBAR_ADD, Length::Px(add.w), Length::Px(add.h))
                    .leaf(preset::SIDEBAR_ADD, [state, (l.sidebar_w as f32).to_bits()])
                    .pressable(ADD_ID, Action::new(ADD, 0));
            });
        }
        let column = f.column();
        let bottom = (helpers > 0).then(|| column.bottom_row());
        describe_rows(ui, f, &column, bottom.as_ref().map_or(f64::INFINITY, |r| r.y_top));
        // over the rows: a later float is on top
        if let Some(r) = bottom {
            ui.float(r.x, r.y_top, |ui| {
                ui.leaf(preset::SIDEBAR_HELPERS, Length::Px(r.w), Length::Px(r.h))
                    .leaf(preset::SIDEBAR_HELPERS, [quiet, 0])
                    .label_with(|out| {
                        // writing to a String cannot fail
                        let _ = strings::write(out, Arg::Helpers(helpers));
                    })
                    .pressable(HELPERS_ID, Action::new(HELPERS, 0));
            });
        }
    });
}

/// The panes' rows, those that end at `bottom` or above it: all text is
/// drawn over all fills, so a name further down would show through.
fn describe_rows(ui: &mut Ui<'_>, f: &SidebarFrame<'_>, column: &Sidebar, bottom: f64) {
    let l = f.layout;
    let quiet = if f.quiet { QUIET } else { 0 };
    let ends_above = |i: usize| {
        let r = column.row_rect(i);
        r.y_top + r.h <= bottom
    };
    let shown = (0..f.rows.count()).take_while(|i| ends_above(*i)).count();
    if shown == 0 {
        return;
    }
    // the last pane cannot be closed
    let can_close = if f.rows.count() > 1 { CAN_CLOSE } else { 0 };
    let first = column.row_rect(0);
    // one column of rows of one height: a point is on the row its
    // height says, and each row's close button is the row's spot,
    // there whether it shows or not (it shows on the pane in use and
    // under the pointer, and takes a press wherever it is)
    ui.float(first.x, first.y_top, |ui| {
        for i in 0..shown {
            let rect = column.row_rect(i);
            let selected = if i == f.focused_idx { SELECTED } else { 0 };
            let close = l.close_session_rects.get(i);
            let (code, tree) = (f.rows.code(i), f.rows.tree(i));
            let name = sidebar::clip_chars(f.rows.label(i), sidebar::name_room(rect, close, code.is_some(), tree, f.cell_w));
            let state = selected | quiet | can_close | state_bits(f.rows.state(i)) << STATE_SHIFT;
            let hollow = if f.rows.hollow(i) { HOLLOW } else { 0 };
            let look = code.map_or(0, |c| HAS_CODE | u32::from(c)) | hollow | tree_bits(tree) << TREE_SHIFT;
            let row = ui
                .leaf(preset::SIDEBAR_ROW, Length::Px(rect.w), Length::Px(rect.h))
                .leaf(preset::SIDEBAR_ROW, [state, look])
                .label_copy(name)
                .pressable(row_id(i), Action::new(ROW, i as u32));
            // a parent's grey row closes nothing: it stands for a pane
            // of another window
            if let Some(c) = close.filter(|_| tree != SidebarTree::Parent) {
                let spot = golia_ui_core::Rect::new(c.x - rect.x, c.y_top - rect.y_top, c.w, c.h);
                row.spot(spot, Action::new(CLOSE, i as u32));
            }
        }
    })
    .rows();
}

/// Whether the pointer lights a control with this state and these bits.
fn lit(data: u32, state: WidgetState) -> bool {
    data & QUIET == 0 && state.hovered
}

pub(crate) fn paint_add(p: &mut ViewPainter<'_>, rect: Rect, data: [u32; 2], state: WidgetState) {
    let sidebar_w = f32::from_bits(data[1]) as f64;
    sidebar::paint_add(p, rect, sidebar_w, data[0] & FULL != 0, lit(data[0], state));
}

pub(crate) fn paint_helpers(p: &mut ViewPainter, rect: Rect, label: &str, data: [u32; 2], state: WidgetState) {
    sidebar::paint_bottom_row(p, rect, label, lit(data[0], state));
}

/// A row, and its close button where the row's spot is: shown on the
/// pane in use and on the row under the pointer, hot under the pointer.
pub(crate) fn paint_row(p: &mut ViewPainter<'_>, rect: Rect, close: Option<Rect>, name: &str, data: [u32; 2], state: WidgetState) {
    let (selected, hovered) = (data[0] & SELECTED != 0, lit(data[0], state));
    let code = (data[1] & HAS_CODE != 0).then_some(data[1] as u16);
    let look = sidebar::RowLook {
        state: state_of((data[0] >> STATE_SHIFT) & 3),
        tree: tree_of(data[1] >> TREE_SHIFT),
        hollow: data[1] & HOLLOW != 0,
        selected,
        hovered,
    };
    sidebar::paint_row(p, rect, code, name, look);
    let lit = hovered;
    if let Some(close) = close.filter(|_| selected || lit) {
        let hot = lit && state.hovered_spot;
        sidebar::paint_close(p, close, sidebar::close_look(data[0] & CAN_CLOSE != 0, hot));
    }
}

/// The sidebar's surface: its tree, its layout and the instances it
/// last painted.
#[derive(Default)]
pub struct SidebarSurface {
    pub kept: KeptSurface,
    /// Shadow panes in all; none, no row.
    pub(crate) helpers: u32,
    described: Option<(u64, (f64, f64))>,
    hit: HitShape,
    /// The generation of the last layout, and the row count, input
    /// found the rows and buttons in the places they were described in.
    ready_for: Option<(u64, usize)>,
    data_rev: u64,
}

impl SidebarSurface {
    fn place(&mut self, f: &SidebarFrame<'_>, window: (f64, f64)) -> Place {
        let now = Some((f.fingerprint(self.helpers), window));
        if self.described != now {
            self.described = now;
            self.hit.keep(f, self.helpers > 0);
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Count `n` shadow panes from here on.
    pub(crate) fn set_helpers(&mut self, n: u32) {
        if self.helpers != n {
            self.helpers = n;
            self.ready_for = None;
        }
    }

    /// Add the sidebar to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, f: &SidebarFrame<'_>) {
        let at = self.place(f, (p.window_w, p.window_h));
        let helpers = self.helpers;
        self.kept.frame(p, at, None, |ui, _| describe(ui, f, helpers));
    }

    /// Lay the sidebar of a window laid out as `layout`, listing
    /// `rows` panes, out again if its rows or buttons have moved since
    /// it was last described, so input that arrives before the next
    /// frame is tested against where that frame will draw them.  The
    /// rows are laid out blank: that frame describes them again.
    pub fn ready(&mut self, font: &mut FontCache, layout: &Layout, rows: usize) {
        if self.ready_for == Some((layout.generation, rows)) {
            return;
        }
        self.ready_for = Some((layout.generation, rows));
        let f = SidebarFrame { layout, rows: &Blank(rows), focused_idx: 0, quiet: false, cell_w: font.cell_w as f32 };
        if self.hit.is(&f, self.helpers > 0) {
            return;
        }
        let at = self.place(&f, (layout.window_w, layout.window_h));
        let helpers = self.helpers;
        self.kept.lay_out(font, at, |ui, _| describe(ui, &f, helpers));
    }
}
