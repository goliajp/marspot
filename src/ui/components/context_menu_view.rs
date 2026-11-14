//! The context menu as a tree: a float over the window holding one
//! node per row.
//!
//! Where the menu stands and how large it is are `context_menu`'s;
//! this file says what is in it and paints its frame, its rows and
//! its dividers.  A press on a row comes back as [`ROW`] with the
//! row's index.

use std::cell::RefCell;
use std::rc::Rc;

use crate::font_cache::FontCache;
use crate::frame_build::color::rgba8_of_f32;
use crate::glyph_atlas::GlyphKey;
use crate::ui::components::context_menu::{self, ContextMenuHit, MenuItem};
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::theme::PanelText;
use crate::ui::theme::spec::{card, chrome, panel};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, preset};
use golia_ui_core::scene::UiRectInstance;
use golia_ui_core::{Action, Edges, GroupKind, Length, NodeId, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;

/// A row was pressed: the argument is its index among the items.
pub const ROW: u32 = 1;
/// The rows, as one widget.
const ROWS: WidgetId = WidgetId(1);
/// Between a row's label and its shortcut in the node's text.
const HINT_MARK: char = '\u{1f}';
/// The label's ascent as a fraction of its point size: what the row
/// centres the capitals against.
const LABEL_ASCENT_RATIO: f64 = 0.75;

/// The menu standing at `frame`, device pixels from the window's
/// corner.  The menu's spec is in window units, half a point each.
pub fn describe(ui: &mut Ui<'_>, frame: Rect, items: &[MenuItem]) {
    let none = Length::ZERO;
    let w = Length::Px(frame.w);
    ui.vstack(|ui| {
        ui.float(frame.x, frame.y_top, |ui| {
            ui.group(ROWS, GroupKind::List, true, |ui| {
                for (i, it) in items.iter().enumerate() {
                    if it.divider {
                        ui.leaf(preset::MENU_DIVIDER, w, Length::Pt(chrome::MENU_DIVIDER_H * 0.5));
                        continue;
                    }
                    ui.leaf(preset::MENU_ROW, w, Length::Pt(chrome::MENU_ROW_H * 0.5))
                        .label_with(|text| {
                            text.push_str(&it.label);
                            if !it.shortcut_hint.is_empty() {
                                text.push(HINT_MARK);
                                text.push_str(&it.shortcut_hint);
                            }
                        })
                        .item(Action::new(ROW, i as u32))
                        .disabled(!it.enabled);
                }
            });
        })
        .width(w)
        .height(Length::Px(frame.h))
        .padding(Edges { top: Length::Pt(chrome::MENU_PAD_Y * 0.5), right: none, bottom: none, left: none })
        .leaf(preset::MENU_FRAME, [0, 0]);
    });
}

/// The menu's frame: the fill, the hairline round it and the shadow
/// under it.
pub(crate) fn paint_frame(p: &mut ViewPainter<'_>, r: Rect) {
    p.ui_rects.push(UiRectInstance {
        origin: [r.x as f32, r.y_top as f32],
        size: [r.w as f32, r.h as f32],
        fill: rgba8_of_f32(palette::frame_bg()),
        border: rgba8_of_f32(palette::frame_border()),
        radius: chrome::px(chrome::POPOVER_RADIUS) as f32,
        border_width: panel::BORDER_PX,
        shadow_offset: [0.0, 0.0],
        shadow_color: rgba8_of_f32(palette::frame_shadow()),
        shadow_blur: chrome::px(chrome::POPOVER_SHADOW_BLUR) as f32,
    });
}

/// A divider: the frame's own border colour across the middle of its
/// row.
pub(crate) fn paint_divider(p: &mut ViewPainter<'_>, r: Rect) {
    let pad = chrome::px(chrome::MENU_PAD_X);
    let (from, to) = (r.x + pad, r.x + r.w - pad);
    let line = Rect { x: from, y_top: r.y_top + r.h * 0.5 - card::SEPARATOR_PX * 0.5, w: to - from, h: card::SEPARATOR_PX };
    p.fill_rounded_rect(line, palette::frame_border(), 0.0, (palette::none(), 0.0));
}

/// A row: the band behind it while the pointer is on it, its label,
/// and its shortcut against the right edge.
pub(crate) fn paint_row(p: &mut ViewPainter<'_>, r: Rect, text: &str, state: WidgetState) {
    let (label, hint) = text.split_once(HINT_MARK).unwrap_or((text, ""));
    let pad = chrome::px(chrome::MENU_PAD_X);
    if state.hovered {
        let inset = chrome::px(chrome::POPOVER_ROW_INSET);
        let band = Rect { x: r.x + inset, y_top: r.y_top, w: r.w - 2.0 * inset, h: r.h };
        p.fill_rounded_rect(band, palette::row_hover(), chrome::px(chrome::POPOVER_ROW_RADIUS) as f32, (palette::none(), 0.0));
    }
    // The label is prose, in the role every panel sets an item in.
    // Its capitals are centred on the row from the top of its em box:
    // `centre - (ascent - cap / 2)`, then down by the ascent the font
    // itself reports.
    let role = PanelText::Item;
    let px = ViewPainter::px_per_pt();
    let size_pt = GlyphKey::size_q_for(role.pt()) as f64 / 4.0;
    let top = r.y_top + r.h * 0.5 - (LABEL_ASCENT_RATIO * role.pt() - role.cap() * 0.5) * px;
    let opts = crate::font_shape::ShapeOptions::default();
    if let Some(at) = p.font.shape_ui_kept(label, role.weight(), opts, size_pt) {
        let face = p.font.shaped_ui(label, at, opts).first().map_or(0, |g| g.font_id as usize);
        let ascent = (p.font.font_ascent(face) * px) as f32;
        let fg = if state.disabled { palette::disabled_fg() } else { palette::fg() };
        p.ui_text_at((r.x + pad) as f32, top as f32 + ascent, label, size_pt, role.weight(), fg);
    }
    if !hint.is_empty() {
        // a cluster of key symbols, not prose: in the mono cell, where
        // each keeps its own width
        let w = hint.chars().count() as f64 * p.cell_w as f64;
        let top = r.y_top + (r.h - p.cell_h as f64) * 0.5;
        p.text((r.x + r.w - pad - w) as f32, top as f32 + p.ascent, hint, palette::fg_muted());
    }
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
}

/// The menu's surface: its tree, its layout, what the pointer has done
/// to it, and the instances it last painted.
pub struct MenuSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
    /// Where the menu stands in the window.
    frame: Rect,
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
        });
        if !same {
            self.frame = context_menu::frame_of(
                window.0, window.1, scale, m.anchor_phys.0, m.anchor_phys.1, m.top_inset, &m.items,
            );
            self.described = Some(Described {
                items: m.items.clone(),
                anchor: m.anchor_phys,
                top_inset: m.top_inset,
                window,
                scale,
            });
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (0.0, 0.0), avail: window }
    }

    /// Bring the tree and its layout up to `m` without painting, so
    /// that a press or the pointer is tested against the menu as the
    /// next frame will show it.
    pub fn lay_out(&mut self, font: &mut FontCache, m: &MenuRender, window: (f64, f64)) {
        let at = self.place(m, window);
        let frame = self.frame;
        self.kept.lay_out(font, at, |ui, _| describe(ui, frame, &m.items));
    }

    /// Bring the kept instances up to `m`, adding nothing to the frame
    /// `p` is painting: the menu is drawn over the finished frame from
    /// [`Self::kept`].
    pub fn refresh(&mut self, p: &mut ViewPainter<'_>, m: &MenuRender) {
        let at = self.place(m, (p.window_w, p.window_h));
        let frame = self.frame;
        if self.kept.refresh(p, at, m.pointer, |ui, _| describe(ui, frame, &m.items)) {
            self.painted_rev += 1;
        }
    }

    /// Add the menu to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, m: &MenuRender) {
        let at = self.place(m, (p.window_w, p.window_h));
        let frame = self.frame;
        self.kept.frame(p, at, m.pointer, |ui, _| describe(ui, frame, &m.items));
    }

    /// Where the menu stands in the window it was last laid out in.
    pub fn frame_rect(&self) -> Rect {
        self.frame
    }

    /// The middle of row `i` in the layout last made: where to put the
    /// pointer to have it on that row.
    pub fn row_centre(&self, i: usize) -> Option<(f64, f64)> {
        let s = &self.kept.surface;
        let at = s.tree().nodes().iter().position(|n| n.leaf_tag == preset::MENU_ROW && n.action.arg == i as u32)?;
        let r = s.layout().rect(NodeId(at as u32));
        Some((r.x + r.w * 0.5, r.y + r.h * 0.5))
    }

    /// Which picture the kept instances are: moves when they change.
    pub fn painted_rev(&self) -> u64 {
        self.painted_rev
    }

    /// The pointer is at `(x, y)`.  True when the picture changed.
    pub fn pointer_move(&mut self, x: f64, y: f64) -> bool {
        self.kept.surface.pointer_move((x, y))
    }

    /// What a press at `(x, y)` lands on: a row that acts, the menu
    /// where nothing acts (a divider, a disabled row, its padding), or
    /// the window outside it.
    pub fn press(&mut self, x: f64, y: f64) -> ContextMenuHit {
        if !self.frame.contains(x, y) {
            return ContextMenuHit::Outside;
        }
        let s = &mut self.kept.surface;
        s.pointer_down((x, y));
        match s.pointer_up((x, y)) {
            Some(a) if a.id == ROW => ContextMenuHit::Item(a.arg as usize),
            _ => ContextMenuHit::Frame,
        }
    }
}
