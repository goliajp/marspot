//! The agent-accounts panel as a tree: the Accounts group with a card
//! per account, wrapped into rows, and the Availability group with
//! the timeline of when each account's windows renew.
//!
//! Where everything in a card is comes from `cc_usage_layout`, on the
//! terminal font's cell; each name, address, label, percentage and bar
//! is a node standing on the rectangle that layout gives it.  The
//! timeline is a plot: one node, drawn by `cc_usage_paint`.

use crate::ui::components::cc_usage_layout::{card_rows, cards_and_timeline, Cell, Shape};
use crate::ui::components::cc_usage_modal::{fit_measured_into, metric, CcUsageAccountRender, CcUsageRender, Clock};
use crate::ui::components::cc_usage_paint as paint;
use crate::ui::components::controls::{self, Measure};
use crate::ui::components::panel_chrome::PanelChrome;
use crate::ui::core::text_fit::cells;
use crate::ui::core::ViewPainter;
use crate::ui::strings;
use crate::ui::theme::spec::{self, card as card_spec, content};
use crate::ui::theme::PanelText;
use crate::ui::tree::kept::Place;
use crate::ui::tree::{FontMeasure, KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{Align, AlignCross, Edges, Length, TextFont, Ui};
use marspot_term::layout::Rect;

/// The timeline's card.
pub const TIMELINE: u32 = preset::CUSTOM + 1;

/// A gap of `px` pixels down a column.
fn down(ui: &mut Ui<'_>, px: f64) {
    ui.leaf(0, Length::ZERO, Length::Px(px.max(0.0)));
}

/// `f`'s node with its top-left corner `at` pixels from the corner of
/// the stack it is laid over.
fn placed(ui: &mut Ui<'_>, at: (f64, f64), f: impl FnOnce(&mut Ui<'_>)) {
    let none = Length::ZERO;
    ui.vstack(f).padding(Edges {
        top: Length::Px(at.1.max(0.0)),
        right: none,
        bottom: none,
        left: Length::Px(at.0.max(0.0)),
    });
}

/// `s` in `role` as `m` measures it, the way the painter does.
fn width(m: Measure<'_>, role: PanelText, s: &str) -> f64 {
    m(s, role.pt(), role.weight()) as f32 as f64
}

/// What a line of a card is set in and where it stands.
struct Line {
    role: PanelText,
    ink: Tone,
    left: f64,
    baseline: f64,
    max_w: f64,
}

/// `text` cut to the line's width with an ellipsis, on its baseline.
fn line(ui: &mut Ui<'_>, m: Measure<'_>, l: Line, text: &str, scratch: &mut String) {
    fit_measured_into(text, l.max_w, &mut |t| width(&mut *m, l.role, t), scratch);
    if scratch.is_empty() {
        return;
    }
    placed(ui, (l.left, l.baseline - spec::px(l.role.cap())), |ui| {
        ui.text_copy(font_of(l.role), l.ink.into(), scratch);
    });
}

/// A usage bar: the track over the whole of `rect`, the colour over
/// the used part of it.  No reading leaves the track alone.
fn bar(ui: &mut Ui<'_>, rect: Rect, util: Option<f32>) {
    if rect.w < metric::BAR_MIN_PX {
        return;
    }
    let mut part = |w: f64, ink: Tone| {
        placed(ui, (rect.x, rect.y_top), |ui| {
            ui.leaf(0, Length::Px(w), Length::Px(rect.h)).fill(ink.into()).radius(Length::Pt(metric::BAR_RADIUS));
        });
    };
    part(rect.w, Tone::Track);
    let Some(util) = util else { return };
    let w = rect.w * (util as f64).clamp(0.0, 1.0);
    if w >= metric::BAR_MIN_PX {
        part(w, [Tone::Ok, Tone::Warn, Tone::Danger][paint::util_severity(util) as usize]);
    }
}

/// One account on `card`, a rectangle measured from the corner of the
/// stack the cards are laid over: name and status, address, a bar per
/// metered window, when the next window renews.
fn account(
    ui: &mut Ui<'_>,
    m: Measure<'_>,
    a: &CcUsageAccountRender,
    card: Rect,
    (cell, window_rows): (Cell, usize),
    scratch: &mut String,
) {
    placed(ui, (card.x, card.y_top), |ui| {
        ui.leaf(0, Length::Px(card.w), Length::Px(card.h))
            .fill(Tone::CardBg.into())
            .border(Length::Px(card_spec::BORDER_PX as f64), Tone::CardBorder.into())
            .radius(Length::Pt(card_spec::RADIUS));
    });
    let rows = card_rows(card, cell.h, window_rows);

    // The chip takes what it needs first; the name gets the rest of
    // the line, so no pairing of lengths can make them collide.
    let mut name_w = rows.right - rows.left;
    if !a.status_label.is_empty() {
        scratch.clear();
        scratch.extend(a.status_label.chars().map(|c| c.to_ascii_uppercase()));
        let chip = rows.chip(controls::chip_size(scratch, &mut *m));
        placed(ui, (chip.x, chip.y_top), |ui| {
            ui.leaf(preset::CHIP, Length::Px(chip.w), Length::Px(chip.h))
                .leaf(preset::CHIP, [a.status_severity as u32, 0])
                .label_copy(scratch);
        });
        name_w = chip.x - spec::px(metric::NAME_TO_CHIP) - rows.left;
    }
    fit_measured_into(&a.name, name_w, &mut |t| width(&mut *m, metric::NAME, t), scratch);
    if !scratch.is_empty() {
        placed(ui, (rows.left, rows.name_line.y_top), |ui| {
            ui.text_copy(font_of(metric::NAME), Tone::Fg.into(), scratch)
                .height(Length::Px(rows.name_line.h))
                .align(Align::Leading);
        });
    }
    let (left, max_w) = (rows.left, rows.right - rows.left);
    let email = Line { role: metric::EMAIL, ink: Tone::FgSecondary, left, baseline: rows.email_baseline, max_w };
    line(ui, &mut *m, email, &a.email, scratch);

    // Labels are not all the same length, so the bars start after the
    // widest one rather than at a fixed column.
    let label_cols = a.windows.iter().map(|w| w.label.chars().count()).max().unwrap_or(0);
    for (i, w) in a.windows.iter().enumerate() {
        let row = rows.bar_row(i, label_cols, cell);
        placed(ui, (rows.left, row.top), |ui| {
            ui.text_copy(TextFont::Mono, Tone::FgMuted.into(), &w.label)
                .height(Length::Px(row.h))
                .align(Align::Leading);
        });
        scratch.clear();
        w.write_percent(scratch);
        let ink = w.util.map_or(Tone::FgMuted, |u| [Tone::OkText, Tone::WarnText, Tone::DangerText][paint::util_severity(u) as usize]);
        placed(ui, (row.pct_right - cells(scratch) as f64 * cell.w, row.top), |ui| {
            ui.text_copy(TextFont::Mono, ink.into(), scratch).height(Length::Px(row.h)).align(Align::Leading);
        });
        bar(ui, row.bar, w.util);
    }

    let reset = Line { role: metric::RESET, ink: Tone::FgMuted, left, baseline: rows.reset_baseline, max_w };
    line(ui, m, reset, &a.reset_label, scratch);
}

/// The sentence shown before either feed has been read, in the middle
/// of an area `w` by `h` pixels.
fn empty(ui: &mut Ui<'_>, m: Measure<'_>, (w, h): (f64, f64), scratch: &mut String) {
    let role = metric::EMPTY;
    fit_measured_into(strings::t(metric::EMPTY_TEXT), w, &mut |t| width(&mut *m, role, t), scratch);
    let left = (w - width(m, role, scratch)) * 0.5;
    ui.zstack(Align::TopLeading, |ui| {
        placed(ui, (left, 0.0), |ui| {
            ui.text_copy(font_of(role), Tone::FgSecondary.into(), scratch)
                .height(Length::Px(h))
                .align(Align::Leading);
        });
    })
    .width(Length::Pct(1.0))
    .height(Length::Px(h));
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
pub fn describe(
    ui: &mut Ui<'_>,
    fm: &FontMeasure<'_>,
    frame: (f64, f64),
    backdrop_top: f64,
    cc: &CcUsageRender,
    cell: Cell,
    scratch: &mut String,
) {
    // where things go, measured from the frame's own corner
    let area = PanelChrome::new(Rect { x: 0.0, y_top: 0.0, w: frame.0, h: frame.1 }, 0.0).content;
    let meta = (!cc.updated_label.is_empty()).then_some(cc.updated_label.as_str());
    panel::panel(ui, frame, backdrop_top, strings::t(metric::TITLE), meta, |ui| {
        let m: Measure<'_> = &mut fm.for_controls();
        if cc.accounts.is_empty() {
            empty(ui, m, (area.w, area.h), scratch);
            return;
        }
        let shape = Shape::of(&cc.accounts);
        let (cards, timeline) = cards_and_timeline(area, cell, &shape);
        heading(ui, strings::t(metric::ACCOUNTS_HEADING), Some(cc.accounts.len()));
        let mut y = area.y_top + spec::px(content::HEADING.cap());
        // the cards over one corner, each on its own rectangle
        let mut cards = cards.peekable();
        if let Some(first) = cards.peek() {
            let top = first.y_top;
            let mut bottom = top;
            down(ui, top - y);
            let over = ui.zstack(Align::TopLeading, |ui| {
                for (a, card) in cc.accounts.iter().zip(cards) {
                    bottom = card.y_top + card.h;
                    let card = Rect { x: card.x - area.x, y_top: card.y_top - top, ..card };
                    account(ui, &mut *m, a, card, (cell, shape.window_rows), scratch);
                }
            });
            over.width(Length::Pct(1.0)).height(Length::Px(bottom - top));
            y = bottom;
        }
        if let Some(t) = &timeline {
            down(ui, t.heading_top - y);
            heading(ui, strings::t(metric::TIMELINE_HEADING), None);
            y = t.heading_top + spec::px(content::HEADING.cap());
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
    scratch: String,
    /// What the timeline's labels are composed in.
    labels: String,
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
            // the copy kept from last time is refreshed where it is
            let d = self.described.get_or_insert_with(|| Described {
                frame: cc.rect,
                backdrop_top: 0.0,
                cell: key,
                updated_label: String::new(),
                accounts: Vec::new(),
                clock: cc.clock,
            });
            (d.frame, d.backdrop_top, d.cell, d.clock) = (cc.rect, top_inset, key, cc.clock);
            d.updated_label.clone_from(&cc.updated_label);
            d.accounts.clone_from(&cc.accounts);
            self.data_rev += 1;
        }
        let size = (cc.rect.w, cc.rect.h);
        let at = Place { data_rev: self.data_rev, origin: (cc.rect.x, cc.rect.y_top), avail: size };
        // the timeline is drawn on the rect the layout gives it in the
        // window, the one its node stands on
        let area = PanelChrome::new(cc.rect, top_inset).content;
        let labels = &mut self.labels;
        let mut inside = |p: &mut ViewPainter<'_>, _tag: u32, _data: [u32; 2]| {
            if let Some(t) = &cards_and_timeline(area, cell, &Shape::of(&cc.accounts)).1 {
                paint::paint_timeline(p, cc, t, cell, labels)
            }
        };
        let scratch = &mut self.scratch;
        self.kept.frame_with(
            p,
            at,
            cc.pointer,
            |ui, fm| describe(ui, fm, size, top_inset, cc, cell, scratch),
            &mut inside,
        );
    }
}
