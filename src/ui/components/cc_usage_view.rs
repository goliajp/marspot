//! The agent-accounts panel as a tree: the Accounts group with a card
//! per account, wrapped into rows, and the Availability group with
//! the timeline of when each account's windows renew.
//!
//! A card and the timeline are each one node.  What is inside them is
//! tabular data and a plot, placed by `cc_usage_layout` on the
//! terminal font's cell and drawn by `cc_usage_paint`; the tree says
//! where each of them is, keeps the picture, and takes the pointer.

use crate::ui::components::cc_usage_layout::{layout, Cell, Shape};
use crate::ui::components::cc_usage_modal::{metric, CcUsageAccountRender, CcUsageRender, Clock};
use crate::ui::components::cc_usage_paint as paint;
use crate::ui::components::panel_chrome::PanelChrome;
use crate::ui::core::ViewPainter;
use crate::ui::strings;
use crate::ui::theme::spec::content;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{AlignCross, Length, Ui};
use marspot_term::layout::Rect;

/// One account's card.  `data[0]` is the account's place in the list.
pub const CARD: u32 = preset::CUSTOM;
/// The timeline's card.
pub const TIMELINE: u32 = preset::CUSTOM + 1;
/// The sentence shown before either feed has been read.
pub const EMPTY: u32 = preset::CUSTOM + 2;

/// A gap of `px` pixels down a column.
fn down(ui: &mut Ui<'_>, px: f64) {
    ui.leaf(0, Length::ZERO, Length::Px(px.max(0.0)));
}

/// A group's heading, with what trails it at the right edge.
fn heading(ui: &mut Ui<'_>, title: &'static str, trailing: Option<usize>) {
    ui.hstack(|ui| {
        ui.text(font_of(content::HEADING), Tone::Fg.into(), title);
        ui.spacer();
        if let Some(n) = trailing {
            ui.text_fmt(font_of(content::HEADING_TRAILING), Tone::FgMuted.into(), format_args!("{n}"));
        }
    })
    .width(Length::Pct(1.0))
    .height(Length::Pt(content::HEADING.cap()))
    .align_cross(AlignCross::End);
}

/// The panel `frame` pixels in size, showing `cc`.
pub fn describe(ui: &mut Ui<'_>, frame: (f64, f64), backdrop_top: f64, cc: &CcUsageRender, cell: Cell) {
    // where things go, measured from the frame's own corner
    let area = PanelChrome::new(Rect { x: 0.0, y_top: 0.0, w: frame.0, h: frame.1 }, 0.0).content;
    let meta = (!cc.updated_label.is_empty()).then_some(cc.updated_label.as_str());
    panel::panel(ui, frame, backdrop_top, strings::t(metric::TITLE), meta, |ui| {
        if cc.accounts.is_empty() {
            ui.leaf(EMPTY, Length::Pct(1.0), Length::Px(area.h));
            return;
        }
        let l = layout(area, cell, &Shape::of(&cc.accounts));
        down(ui, l.heading_top - area.y_top);
        heading(ui, strings::t(metric::ACCOUNTS_HEADING), Some(cc.accounts.len()));
        let mut y = l.heading_top + crate::ui::theme::spec::px(content::HEADING.cap());
        // the cards a row at a time: a new row starts where a card is
        // further down than the one before it
        let mut at = 0;
        while at < l.cards.len() {
            let top = l.cards[at].y_top;
            let end = at + l.cards[at..].iter().take_while(|c| c.y_top == top).count();
            down(ui, top - y);
            ui.hstack(|ui| {
                let mut x = area.x;
                for (i, card) in l.cards[at..end].iter().enumerate() {
                    ui.leaf(0, Length::Px((card.x - x).max(0.0)), Length::ZERO);
                    ui.leaf(CARD, Length::Px(card.w), Length::Px(card.h)).leaf(CARD, [(at + i) as u32, 0]);
                    x = card.x + card.w;
                }
            });
            y = top + l.cards[at].h;
            at = end;
        }
        if let Some(t) = &l.timeline {
            down(ui, t.heading_top - y);
            heading(ui, strings::t(metric::TIMELINE_HEADING), None);
            y = t.heading_top + crate::ui::theme::spec::px(content::HEADING.cap());
            down(ui, t.card.y_top - y);
            ui.leaf(TIMELINE, Length::Px(t.card.w), Length::Px(t.card.h));
        }
    });
}

/// What the panel was last described from.
struct Described {
    frame: Rect,
    backdrop_top: f64,
    cell: (f64, f64, f64),
    updated_label: String,
    accounts: Vec<CcUsageAccountRender>,
    clock: Clock,
}

/// The accounts panel's surface.
#[derive(Default)]
pub struct CcUsageSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
}

impl CcUsageSurface {
    /// Add the panel to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, cc: &CcUsageRender, top_inset: f64) {
        let cell = Cell { w: p.cell_w as f64, h: p.cell_h as f64, ascent: p.ascent as f64 };
        let key = (cell.w, cell.h, cell.ascent);
        let same = self.described.as_ref().is_some_and(|d| {
            d.frame == cc.rect
                && d.backdrop_top == top_inset
                && d.cell == key
                && d.clock == cc.clock
                && d.updated_label == cc.updated_label
                && d.accounts == cc.accounts
        });
        if !same {
            self.described = Some(Described {
                frame: cc.rect,
                backdrop_top: top_inset,
                cell: key,
                updated_label: cc.updated_label.clone(),
                accounts: cc.accounts.clone(),
                clock: cc.clock,
            });
            self.data_rev += 1;
        }
        let size = (cc.rect.w, cc.rect.h);
        let at = Place { data_rev: self.data_rev, origin: (cc.rect.x, cc.rect.y_top), avail: size };
        // a card and the timeline are drawn on the rects the layout
        // gives them in the window, the ones their nodes stand on
        let area = PanelChrome::new(cc.rect, top_inset).content;
        let mut inside = |p: &mut ViewPainter<'_>, tag: u32, data: [u32; 2]| {
            if tag == EMPTY {
                return paint::paint_empty(p, area);
            }
            let shape = Shape::of(&cc.accounts);
            let l = layout(area, cell, &shape);
            match tag {
                CARD => {
                    let i = data[0] as usize;
                    paint::paint_card(p, &cc.accounts[i], l.cards[i], cell, shape.window_rows)
                }
                TIMELINE => {
                    if let Some(t) = &l.timeline {
                        paint::paint_timeline(p, cc, t, cell)
                    }
                }
                // no other tag is described
                _ => unreachable!(),
            }
        };
        self.kept.frame_with(p, at, cc.pointer, |ui, _| describe(ui, size, top_inset, cc, cell), &mut inside);
    }
}
