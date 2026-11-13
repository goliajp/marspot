//! The keyboard shortcuts panel as a tree: two columns of groups, each
//! group a heading over a card of rows, each row a command's name and
//! its chord in key caps.
//!
//! The rows are the command table's.  What does not fit the content
//! area whole is left out when the tree is described, by the rule
//! `shortcuts_panel::walk` states: a row that would cross the bottom
//! edge is not there, its card ends at that edge, and a group whose
//! first row would cross it has no heading either.

use std::cell::RefCell;
use std::fmt::Write;
use std::rc::Rc;

use crate::commands::{self, Keys, Reference};
use crate::font_cache::FontCache;
use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::shortcuts_panel::{COLUMNS, metric};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{Msg, t};
use crate::ui::theme::spec::{self, card, content, control, space};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{AlignCross, Edges, Length, Ui};
use marspot_term::layout::Rect;

/// Between two parts of a row's keys in the label a key-cap node
/// carries.
const PART: char = '\t';

/// The panel `frame` pixels in size, its key caps set for a terminal
/// cell `cell` pixels in size.  `rows` is what the command table's
/// reference is read into.
pub fn describe(ui: &mut Ui<'_>, frame: (f64, f64), backdrop_top: f64, cell: (f64, f64), rows: &mut Vec<Reference>) {
    let area = PanelChrome::new(Rect { x: 0.0, y_top: 0.0, w: frame.0, h: frame.1 }, 0.0).content;
    let bottom = area.y_top + area.h + 1e-9;
    let gap = spec::px(metric::COLUMN_GAP);
    let col_w = spec::snap_down((area.w - gap) * 0.5).max(0.0);
    let cap_h = controls::keycap_size("", 0.0, cell.1).1;
    let row_h = cap_h + 2.0 * spec::px(metric::ROW_PAD_Y);
    commands::reference_into(rows);
    let rows = &*rows;

    panel::panel(ui, frame, backdrop_top, t(Msg::PanelShortcuts), None, |ui| {
        ui.hstack(|ui| {
            for menus in COLUMNS {
                ui.vstack(|ui| {
                    let mut y = area.y_top;
                    for (gi, menu) in menus.iter().enumerate() {
                        let lead = if gi > 0 { spec::px(space::CARD_TO_HEADING) } else { 0.0 };
                        let in_menu = rows.iter().filter(|r| r.menu == *menu);
                        let card_top = y + lead + panel_chrome::heading_height();
                        if card_top + row_h > bottom {
                            break;
                        }
                        let full_h = in_menu.clone().count() as f64 * row_h;
                        ui.leaf(0, Length::ZERO, Length::Px(lead));
                        ui.text(font_of(content::HEADING), Tone::Fg.into(), t(menu.title()));
                        ui.leaf(0, Length::ZERO, Length::Pt(space::HEADING_TO_CARD));
                        ui.vstack(|ui| {
                            for (ri, r) in in_menu.enumerate() {
                                if card_top + (ri + 1) as f64 * row_h > bottom {
                                    break;
                                }
                                if ri > 0 {
                                    ui.rule(Length::Px(card::SEPARATOR_PX), Tone::Separator.into())
                                        .padding(Edges::xy(Length::Pt(card::SEPARATOR_INSET), Length::ZERO));
                                }
                                row(ui, r.title, r.keys, row_h, cap_h, cell);
                            }
                        })
                        .width(Length::Pct(1.0))
                        .height(Length::Px(full_h.min(bottom - card_top)))
                        .fill(Tone::CardBg.into())
                        .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
                        .radius(Length::Pt(card::RADIUS));
                        y = card_top + full_h;
                    }
                })
                .width(Length::Px(col_w));
            }
        })
        .gap(Length::Px(gap));
    });
}

/// One row: the command's name at the left, its keys at the right.
fn row(ui: &mut Ui<'_>, title: Msg, keys: Keys, row_h: f64, cap_h: f64, cell: (f64, f64)) {
    ui.hstack(|ui| {
        ui.text(font_of(metric::TITLE), Tone::Fg.into(), t(title));
        ui.spacer();
        // one node for the row's caps: they are set right to left from
        // its right edge, each as wide as its glyphs measure
        let mut label = KeysLabel::default();
        let (dash, w) = match keys {
            Keys::One(a) => {
                let _ = write!(label, "{a}");
                (0, cap_w(label.as_str(), cell))
            }
            Keys::Either(a, b) | Keys::Range(a, b) => {
                let range = matches!(keys, Keys::Range(..));
                let _ = write!(label, "{a}");
                let first = cap_w(label.as_str(), cell);
                let at = label.len;
                let _ = if range { write!(label, "{PART}–{PART}{b}") } else { write!(label, "{PART}{b}") };
                let last = cap_w(&label.as_str()[label.as_str().rfind(PART).map_or(at, |i| i + 1)..], cell);
                let between =
                    if range { cap_w("–", cell) } else { spec::px(control::KEYCAP_GAP) };
                (range as u32 * 0b10, first + between + last)
            }
        };
        ui.leaf(preset::KEYS, Length::Px(w), Length::Px(cap_h)).leaf(preset::KEYS, [dash, 0]).label_copy(label.as_str());
    })
    .width(Length::Pct(1.0))
    .height(Length::Px(row_h))
    .padding(Edges::xy(Length::Pt(card::ROW_PAD_X), Length::ZERO))
    .align_cross(AlignCross::Center);
}

fn cap_w(label: &str, cell: (f64, f64)) -> f64 {
    controls::keycap_size(label, cell.0, cell.1).0
}

/// A row's keys written out, on the stack: the longest is a range of
/// two chords.
struct KeysLabel {
    buf: [u8; 48],
    len: usize,
}

impl Default for KeysLabel {
    fn default() -> Self {
        Self { buf: [0; 48], len: 0 }
    }
}

impl KeysLabel {
    fn as_str(&self) -> &str {
        // only whole strings are appended
        std::str::from_utf8(&self.buf[..self.len]).expect("whole strings")
    }
}

impl Write for KeysLabel {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Paint the caps of a key-cap node: `label` is its parts, `dash` the
/// bit set of parts that are glyphs with no cap behind them.
pub(crate) fn paint_keys(p: &mut ViewPainter, rect: Rect, label: &str, dash: u32) {
    let gap = spec::px(control::KEYCAP_GAP);
    let mut right = rect.x + rect.w;
    let mut after_filled = false;
    let parts = label.split(PART).count();
    for (n, part) in label.rsplit(PART).enumerate() {
        let filled = dash & (1 << (parts - 1 - n)) == 0;
        if after_filled && filled {
            right -= gap;
        }
        right = controls::paint_keycap_ending_at(p, right, rect.y_top, rect.h, part, filled);
        after_filled = filled;
    }
}

/// The panel's render data: its frame, and the surface it is drawn
/// through.  The rows are the command table's and need no snapshot.
#[derive(Clone)]
pub struct ShortcutsRender {
    pub rect: Rect,
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: Rc<RefCell<ShortcutsSurface>>,
}

impl ShortcutsRender {
    /// A panel at `rect` on a surface of its own.
    pub fn new(rect: Rect) -> Self {
        Self { rect, pointer: None, surface: Default::default() }
    }
}

/// The shortcuts panel's surface.
#[derive(Default)]
pub struct ShortcutsSurface {
    pub kept: KeptSurface,
    /// The frame size, the dimmed band's top and the cell it was last
    /// described for.
    described: Option<((f64, f64), f64, (f64, f64))>,
    data_rev: u64,
    rows: Vec<Reference>,
}

impl ShortcutsSurface {
    fn place(&mut self, sp: &ShortcutsRender, backdrop_top: f64, cell: (f64, f64)) -> Place {
        let size = (sp.rect.w, sp.rect.h);
        let now = Some((size, backdrop_top, cell));
        if self.described != now {
            self.described = now;
            self.data_rev += 1;
        }
        Place { data_rev: self.data_rev, origin: (sp.rect.x, sp.rect.y_top), avail: size }
    }

    /// Bring the tree and its layout up to `sp` without painting.
    pub fn lay_out(&mut self, font: &mut FontCache, sp: &ShortcutsRender, backdrop_top: f64) {
        // through f32, as a frame is handed it
        let cell = (font.cell_w as f32 as f64, font.cell_h as f32 as f64);
        let at = self.place(sp, backdrop_top, cell);
        let rows = &mut self.rows;
        self.kept.lay_out(font, at, |ui, _| describe(ui, at.avail, backdrop_top, cell, rows));
    }

    /// Add the panel to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, sp: &ShortcutsRender, backdrop_top: f64) {
        let cell = (p.cell_w as f64, p.cell_h as f64);
        let at = self.place(sp, backdrop_top, cell);
        let rows = &mut self.rows;
        self.kept.frame(p, at, sp.pointer, |ui, _| describe(ui, at.avail, backdrop_top, cell, rows));
    }
}
