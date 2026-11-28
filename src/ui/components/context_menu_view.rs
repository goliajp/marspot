//! The context menu as a tree: a float over the window holding one
//! node per row.
//!
//! Where the menu stands and how large it is are `context_menu`'s;
//! this file says what is in it (`context_menu_paint` paints it).  A
//! press on a row comes back as [`ROW`] with the row's index.

use std::cell::RefCell;
use std::rc::Rc;

use crate::font_cache::FontCache;
use crate::ui::components::context_menu::{self, ContextMenuHit, MenuItem};
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::chrome;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::{Action, Edges, GroupKind, Length, NodeId, Ui, WidgetId};
use marspot_term::layout::Rect;

#[path = "context_menu_paint.rs"]
mod paint;
use paint::HINT_MARK;
pub(crate) use paint::{paint_divider, paint_frame, paint_row, paint_tip};

/// A row was pressed: the argument is its index among the items.
pub const ROW: u32 = 1;
/// A row of the menu opened beside a row was pressed: the argument is
/// its index among that row's `submenu`.
pub const SUB_ROW: u32 = 2;
/// The rows, as one widget, and the rows of the menu beside.
const ROWS: WidgetId = WidgetId(1);
const SUB_ROWS: WidgetId = WidgetId(2);
/// Where a shortcut goes on a row that opens a menu beside it.
const OPENS_MARK: &str = "▸";

/// What stands beside the menu: the menu row `.0` opened, at its
/// frame, and the reason a disabled row is off, in its box.
#[derive(Clone, Copy, Default)]
pub struct Beside<'a> {
    pub sub: Option<(usize, Rect)>,
    pub tip: Option<(Rect, &'a str)>,
}

/// The menu standing at `frame`, device pixels from the window's
/// corner, and what stands beside it.  The menu's spec is in window
/// units, half a point each.
pub fn describe(ui: &mut Ui<'_>, frame: Rect, items: &[MenuItem], beside: Beside<'_>) {
    ui.vstack(|ui| {
        menu_float(ui, frame, items, ROWS, ROW, beside.sub.map(|s| s.0));
        if let Some((row, at)) = beside.sub
            && let Some(it) = items.get(row)
        {
            menu_float(ui, at, &it.submenu, SUB_ROWS, SUB_ROW, None);
        }
        if let Some((at, why)) = beside.tip {
            ui.float(at.x, at.y_top, |ui| {
                ui.leaf(preset::MENU_TIP, Length::Px(at.w), Length::Px(at.h)).label_copy(why);
            });
        }
    });
}

/// One menu's frame and rows at `frame`; a row pressed acts as
/// `action` with its index.  A row that opens a menu shows a mark where
/// a shortcut goes, and the row `open` whose menu is open stays lit.
fn menu_float(ui: &mut Ui<'_>, frame: Rect, items: &[MenuItem], id: WidgetId, action: u32, open: Option<usize>) {
    let none = Length::ZERO;
    let w = Length::Px(frame.w);
    ui.float(frame.x, frame.y_top, |ui| {
        ui.group(id, GroupKind::List, true, |ui| {
            for (i, it) in items.iter().enumerate() {
                if it.divider {
                    ui.leaf(preset::MENU_DIVIDER, w, Length::Pt(chrome::MENU_DIVIDER_H));
                    continue;
                }
                ui.leaf(preset::MENU_ROW, w, Length::Pt(chrome::MENU_ROW_H))
                    .leaf(preset::MENU_ROW, [u32::from(open == Some(i)), 0])
                    .label_with(|text| {
                        text.push_str(&it.label);
                        let hint = if it.submenu.is_empty() { it.shortcut_hint.as_str() } else { OPENS_MARK };
                        if !hint.is_empty() {
                            text.push(HINT_MARK);
                            text.push_str(hint);
                        }
                    })
                    .item(Action::new(action, i as u32))
                    .disabled(!it.enabled);
            }
        });
    })
    .width(w)
    .height(Length::Px(frame.h))
    .padding(Edges { top: Length::Pt(chrome::MENU_PAD_Y), right: none, bottom: none, left: none })
    .leaf(preset::MENU_FRAME, [0, 0]);
}

/// An open menu's render data: where it was opened, what is in it,
/// and the surface it is drawn through.
///
/// The surface is shared with whoever takes the menu's input: the
/// layout a press is tested against is the one the frame was painted
/// from.
#[derive(Clone)]
pub struct MenuRender {
    /// Where the menu was asked for, device pixels.
    pub anchor_phys: (f64, f64),
    /// The y the menu must stay under.
    pub top_inset: f64,
    pub items: Rc<[MenuItem]>,
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: Rc<RefCell<MenuSurface>>,
}

impl MenuRender {
    /// A menu of `items` on a surface of its own.
    pub fn new(anchor_phys: (f64, f64), top_inset: f64, items: Vec<MenuItem>) -> Self {
        Self { anchor_phys, top_inset, items: items.into(), pointer: None, surface: Default::default() }
    }
}

/// What the menu was last described from.
struct Described {
    items: Rc<[MenuItem]>,
    anchor: (f64, f64),
    top_inset: f64,
    window: (f64, f64),
    scale: f64,
    open: Option<usize>,
    tip: Option<usize>,
}

/// The menu's surface: its tree, its layout, what the pointer and the
/// keyboard have done to it, and the instances it last painted.
pub struct MenuSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
    /// Where the menu stands in the window, and each of its rows.
    frame: Rect,
    rows: Vec<Rect>,
    /// The row whose menu is open beside it, and where that stands.
    open: Option<usize>,
    sub_frame: Option<Rect>,
    /// The disabled row whose reason is said, and where it is said.
    tip: Option<usize>,
    tip_frame: Option<Rect>,
    /// Moved each time the kept instances are painted again.
    painted_rev: u64,
    /// The kept instances as the renderer last uploaded them.
    pub(crate) buffers: crate::render_metal::MenuBuffers,
}

impl Default for MenuSurface {
    fn default() -> Self {
        Self {
            kept: KeptSurface::new(),
            described: None,
            data_rev: 0,
            frame: Rect { x: 0.0, y_top: 0.0, w: 0.0, h: 0.0 },
            rows: Vec::new(),
            open: None,
            sub_frame: None,
            tip: None,
            tip_frame: None,
            painted_rev: 0,
            buffers: Default::default(),
        }
    }
}

impl MenuSurface {
    fn place(&mut self, m: &MenuRender, window: (f64, f64)) -> Place {
        let scale = crate::ui::chrome_scale();
        let same = self.described.as_ref().is_some_and(|d| {
            Rc::ptr_eq(&d.items, &m.items)
                && d.anchor == m.anchor_phys
                && d.top_inset == m.top_inset
                && d.window == window
                && d.scale == scale
                && d.open == self.open
                && d.tip == self.tip
        });
        if !same {
            self.frame = context_menu::frame_of(
                window.0, window.1, scale, m.anchor_phys.0, m.anchor_phys.1, m.top_inset, &m.items,
            );
            context_menu::rows_into(self.frame, scale, &m.items, &mut self.rows);
            self.place_beside(&m.items, window, m.top_inset);
            self.described = Some(Described {
                items: m.items.clone(),
                anchor: m.anchor_phys,
                top_inset: m.top_inset,
                window,
                scale,
                open: self.open,
                tip: self.tip,
            });
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// What stands beside the menu of `items` as last placed.
    fn beside<'a>(&self, items: &'a [MenuItem]) -> Beside<'a> {
        Beside {
            sub: self.open.zip(self.sub_frame),
            tip: self.tip.zip(self.tip_frame).and_then(|(i, f)| items.get(i).map(|it| (f, it.tooltip.as_str()))),
        }
    }

    /// Bring the tree and its layout up to `m` without painting, so
    /// that a press or the pointer is tested against the menu as the
    /// next frame will show it.
    pub fn lay_out(&mut self, font: &mut FontCache, m: &MenuRender, window: (f64, f64)) {
        let at = self.place(m, window);
        let (frame, beside) = (self.frame, self.beside(&m.items));
        self.kept.lay_out(font, at, |ui, _| describe(ui, frame, &m.items, beside));
    }

    /// Bring the kept instances up to `m`, adding nothing to the frame
    /// `p` is painting: the menu is drawn over the finished frame from
    /// [`Self::kept`].
    pub fn refresh(&mut self, p: &mut ViewPainter<'_>, m: &MenuRender) {
        let at = self.place(m, (p.window_w, p.window_h));
        let (frame, beside) = (self.frame, self.beside(&m.items));
        if self.kept.refresh(p, at, m.pointer, |ui, _| describe(ui, frame, &m.items, beside)) {
            self.painted_rev += 1;
        }
    }

    /// Add the menu to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, m: &MenuRender) {
        let at = self.place(m, (p.window_w, p.window_h));
        let (frame, beside) = (self.frame, self.beside(&m.items));
        self.kept.frame(p, at, m.pointer, |ui, _| describe(ui, frame, &m.items, beside));
    }

    /// Where the menu stands in the window it was last laid out in.
    pub fn frame_rect(&self) -> Rect {
        self.frame
    }

    /// The middle of row `i` in the layout last made: where to put the
    /// pointer to have it on that row.
    pub fn row_centre(&self, i: usize) -> Option<(f64, f64)> {
        self.centre_of(ROW, i)
    }

    /// The middle of row `i` of the menu open beside the menu.
    pub fn sub_row_centre(&self, i: usize) -> Option<(f64, f64)> {
        self.centre_of(SUB_ROW, i)
    }

    fn centre_of(&self, action: u32, i: usize) -> Option<(f64, f64)> {
        let s = &self.kept.surface;
        let at = s.tree().nodes().iter().position(|n| {
            n.leaf_tag == preset::MENU_ROW && n.action.id == action && n.action.arg == i as u32
        })?;
        let r = s.layout().rect(NodeId(at as u32));
        Some((r.x + r.w * 0.5, r.y + r.h * 0.5))
    }

    /// Which picture the kept instances are: moves when they change.
    pub fn painted_rev(&self) -> u64 {
        self.painted_rev
    }
}

#[path = "context_menu_beside.rs"]
mod beside;
pub use beside::MenuKey;
