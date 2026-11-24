//! The helper panel as a tree: a group per session with shadow panes,
//! a card with a row per shadow pane, and under the card what can be
//! done to the session as a whole.
//!
//! A row is the pane's full name over what it is doing, how long it has
//! run, and what can be done to it.  Closing answers a press only: the
//! Tab key does not arrive at ending a session.

use std::cell::RefCell;
use std::rc::Rc;

use crate::font_cache::FontCache;
use crate::ui::components::controls::{self, ButtonKind, Measure};
use crate::ui::components::panel_chrome;
use crate::ui::core::ViewPainter;
use crate::ui::strings::{self, Arg, Msg, t};
use crate::ui::theme::PanelText;
use crate::ui::theme::spec::{self, card, content, control, space};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{FontMeasure, KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{Action, AlignCross, Edges, Length, Ui, WidgetId};
use marspot_term::layout::Rect;

/// The scroll slot the panel's content is in.
pub const SCROLL: u8 = 0;

pub mod metric {
    use crate::ui::theme::{PanelText, spec};

    /// The wide step, as the process monitor.
    pub const PANEL_W: f64 = 720.0;
    pub const ROW_H: f64 = spec::control::BUTTON_H + 2.0 * spec::card::ROW_PAD_Y;
    /// A row with less room than this for its text has buttons below it.
    pub const TEXT_MIN: f64 = 160.0;
    /// Room for how long a pane has run.
    pub const RAN_W: f64 = 72.0;
    pub const NAME: PanelText = PanelText::Label;
    pub const DOING: PanelText = PanelText::Secondary;
}

const MOVE: u32 = 1;
const CLOSE_ONE: u32 = 2;
const CLOSE_ALL: u32 = 3;
const CONTROL: u32 = 4;
const VIEW: u32 = 5;
/// Widget ids: a row's or a group's number from its kind's base.
const IDS_EACH: u32 = 10_000;

/// What the panel was asked to do: `View`, `Move` and `Close` a row by its
/// number from the top, `CloseAll` and `Control` a group by its.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    None,
    View(usize),
    Move(usize),
    Close(usize),
    CloseAll(usize),
    /// Deny or allow the session's directory to control panes.
    Control(usize),
}

pub fn asked(a: Action) -> Ask {
    let i = a.arg as usize;
    match a.id {
        VIEW => Ask::View(i),
        MOVE => Ask::Move(i),
        CLOSE_ONE => Ask::Close(i),
        CLOSE_ALL => Ask::CloseAll(i),
        CONTROL => Ask::Control(i),
        _ => Ask::None,
    }
}

/// One shadow pane.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperRow {
    pub sid: u64,
    /// The pane whose shadow it is.
    pub parent: u64,
    /// The full name messages reach it by.
    pub name: String,
    /// The title the shell gave it, else its directory.
    pub doing: String,
    /// When its shell started; None when that is not known.
    pub started_unix: Option<u64>,
}

/// Whether a session's directory may control panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Control {
    #[default]
    Allowed,
    Denied,
    /// Denied through a directory above it: not this session's to allow.
    DeniedAbove,
}

/// One session and the shadow panes below it.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperGroup {
    pub parent: u64,
    pub name: String,
    /// Whether its directory may control panes.
    pub control: Control,
    pub rows: Vec<HelperRow>,
}

#[derive(Clone)]
pub struct HelpersRender {
    pub rect: Rect,
    pub scroll: f64,
    pub groups: Rc<Vec<HelperGroup>>,
    pub now_unix: u64,
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: Rc<RefCell<HelpersSurface>>,
}

/// The buttons under a card are one above the other: side by side they
/// are wider than a panel `panel_w` wide has room for.
fn stacked(panel_w: f64, m: Measure<'_>) -> bool {
    let mut w = |msg| controls::button_size(t(msg), m).0;
    let both = w(Msg::HelpersCloseAll) + w(Msg::HelpersDeny).max(w(Msg::HelpersAllow));
    both + spec::px(space::CONTROL_TO_CONTROL + 2.0 * content::PAD_X) > panel_w
}

/// What a row's buttons say, and the room beside them for the rest of
/// the row in a panel `panel_w` wide.
fn beside_buttons(panel_w: f64, m: Measure<'_>) -> ([&'static str; 3], f64) {
    let labels = [t(Msg::HelperViewHere), t(Msg::HelperMoveToWindow), t(Msg::HelperClose)];
    let buttons: f64 = labels.map(|label| controls::button_size(label, m).0).iter().sum();
    let pads = spec::px(content::PAD_X + card::ROW_PAD_X);
    (labels, panel_w - buttons - 2.0 * pads - 4.0 * spec::px(space::CONTROL_TO_CONTROL))
}

/// How tall a row is in a panel `panel_w` wide: with no room for its
/// text beside its buttons, all but the first are below it.
fn row_h(panel_w: f64, m: Measure<'_>) -> f64 {
    let fits = beside_buttons(panel_w, m).1 >= spec::px(metric::TEXT_MIN);
    let more = if fits { 0.0 } else { spec::px(control::BUTTON_H) + spec::px(card::ROW_PAD_Y) };
    spec::px(metric::ROW_H) + more
}

/// How tall the content for `groups` is in a panel `panel_w` wide.
pub fn content_h(groups: &[HelperGroup], panel_w: f64, m: Measure<'_>) -> f64 {
    if groups.is_empty() {
        return spec::px(PanelText::Secondary.cap());
    }
    let button_h = spec::px(control::BUTTON_H);
    let both = if stacked(panel_w, m) { 2.0 * button_h + spec::px(space::CONTROL_TO_CONTROL) } else { button_h };
    let row_h = row_h(panel_w, m);
    let group = |g: &HelperGroup| {
        let buttons = if g.control == Control::DeniedAbove { button_h } else { both };
        panel_chrome::heading_height() + g.rows.len() as f64 * row_h + spec::px(space::CARD_TO_FOOTER) + buttons
    };
    groups.iter().map(group).sum::<f64>() + (groups.len() - 1) as f64 * spec::px(space::CARD_TO_HEADING)
}

/// The panel's frame in a window, where it opens.
pub fn panel_rect(window_w: f64, window_h: f64, groups: &[HelperGroup], top_inset: f64, m: Measure<'_>) -> Rect {
    let w = spec::px(metric::PANEL_W);
    let panel_w = panel_chrome::frame_rect(window_w, window_h, w, window_h, top_inset).w;
    let h = panel_chrome::frame_height(spec::snap_up(content_h(groups, panel_w, m)));
    panel_chrome::frame_rect(window_w, window_h, w, h, top_inset)
}

/// `home`, the frame where it would open now, with its top edge kept at
/// `top` while it is open, as far as the window has room below.
pub fn kept_at(home: Rect, top: f64, window_h: f64, top_inset: f64) -> Rect {
    let lowest = (window_h - home.h).max(top_inset);
    Rect { y_top: spec::snap(top.min(lowest)), ..home }
}

/// How far the content can be pushed up.
pub fn max_scroll(rect: Rect, groups: &[HelperGroup], m: Measure<'_>) -> f64 {
    let room = panel_chrome::PanelChrome::new(rect, 0.0).content.h;
    (content_h(groups, rect.w, m) - room).max(0.0)
}

/// A secondary button that answers a press and is no stop for Tab.
fn press_button(ui: &mut Ui<'_>, m: Measure<'_>, id: WidgetId, action: Action, label: &str) {
    let (w, h) = controls::button_size(label, m);
    ui.leaf(preset::BUTTON, Length::Px(w), Length::Px(h))
        .leaf(preset::BUTTON, [(ButtonKind::Secondary) as u32, 0])
        .label_copy(label)
        .radius(Length::Pt(control::BUTTON_RADIUS))
        .pressable(id, action);
}

fn id(kind: u32, i: usize) -> WidgetId {
    WidgetId(kind * IDS_EACH + i as u32)
}

pub fn describe(ui: &mut Ui<'_>, fm: &FontMeasure<'_>, frame: (f64, f64), backdrop_top: f64, groups: &[HelperGroup], now_unix: u64) {
    let mut m = fm.for_controls();
    let gap = Length::Pt(space::CONTROL_TO_CONTROL);
    let stack = stacked(frame.0, &mut m);
    panel::panel(ui, frame, backdrop_top, t(Msg::PanelHelpers), None, |ui| {
        if groups.is_empty() {
            ui.text(font_of(PanelText::Secondary), Tone::FgMuted.into(), t(Msg::HelpersEmpty));
            return;
        }
        ui.scroll(SCROLL, |ui| {
            let mut row_no = 0;
            for (gi, g) in groups.iter().enumerate() {
                if gi > 0 {
                    ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_HEADING));
                }
                ui.text_copy(font_of(content::HEADING), Tone::Fg.into(), &g.name).truncate();
                ui.leaf(0, Length::ZERO, Length::Pt(space::HEADING_TO_CARD));
                ui.vstack(|ui| {
                    for (ri, r) in g.rows.iter().enumerate() {
                        if ri > 0 {
                            ui.rule(Length::Px(card::SEPARATOR_PX), Tone::Separator.into())
                                .padding(Edges::xy(Length::Pt(card::SEPARATOR_INSET), Length::ZERO));
                        }
                        row(ui, &mut m, frame.0, row_no, r, now_unix);
                        row_no += 1;
                    }
                })
                .width(Length::Pct(1.0))
                .fill(Tone::CardBg.into())
                .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
                .radius(Length::Pt(card::RADIUS));
                ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_FOOTER));
                let buttons = |ui: &mut Ui<'_>| {
                    press_button(ui, &mut m, id(CLOSE_ALL, gi), Action::new(CLOSE_ALL, gi as u32), t(Msg::HelpersCloseAll));
                    let control = t(match g.control {
                        Control::Allowed => Msg::HelpersDeny,
                        Control::Denied => Msg::HelpersAllow,
                        Control::DeniedAbove => return,
                    });
                    preset::button(ui, &mut m, id(CONTROL, gi), Action::new(CONTROL, gi as u32), control, ButtonKind::Secondary, false);
                };
                if stack { ui.vstack(buttons) } else { ui.hstack(buttons) }.gap(gap).whole();
            }
        })
        .width(Length::Pct(1.0));
    });
}

/// One row, in a panel `panel_w` wide: number `i` from the top.
fn row(ui: &mut Ui<'_>, m: Measure<'_>, panel_w: f64, i: usize, r: &HelperRow, now_unix: u64) {
    let ([view, move_to, close], room) = beside_buttons(panel_w, m);
    let height = row_h(panel_w, m);
    let below = height > spec::px(metric::ROW_H);
    let view_w = controls::button_size(view, m).0;
    let inside = panel_w - 2.0 * spec::px(content::PAD_X + card::ROW_PAD_X);
    let gaps = spec::px(space::CONTROL_TO_CONTROL);
    let room = if below { inside - view_w - 2.0 * gaps } else { room };
    let ran_w = Some(spec::px(metric::RAN_W)).filter(|w| *w < room);
    let gap = Length::Pt(space::CONTROL_TO_CONTROL);
    let text_w = Length::Px((room - ran_w.unwrap_or(0.0)).max(0.0));
    let text = |ui: &mut Ui<'_>| {
        ui.vstack(|ui| {
            ui.text_copy(font_of(metric::NAME), Tone::Fg.into(), &r.name).truncate();
            ui.text_copy(font_of(metric::DOING), Tone::FgMuted.into(), &r.doing).truncate();
        })
        .width(text_w)
        .gap(Length::Pt(space::S8));
        if let Some(w) = ran_w {
            ui.hstack(|ui| {
                ui.spacer();
                ui.text_with(font_of(metric::DOING), Tone::FgMuted.into(), |out| match r.started_unix {
                    // writing to a String cannot fail
                    Some(at) => drop(strings::write(out, Arg::RanFor { minutes: now_unix.saturating_sub(at) / 60 })),
                    None => out.push('—'),
                });
            })
            .width(Length::Px(w));
        }
    };
    let others = |ui: &mut Ui<'_>, m: Measure<'_>| {
        preset::button(ui, m, id(MOVE, i), Action::new(MOVE, i as u32), move_to, ButtonKind::Secondary, false);
        press_button(ui, m, id(CLOSE_ONE, i), Action::new(CLOSE_ONE, i as u32), close);
    };
    let pad = Edges::xy(Length::Pt(card::ROW_PAD_X), if below { Length::Pt(card::ROW_PAD_Y) } else { Length::ZERO });
    ui.vstack(|ui| {
        ui.hstack(|ui| {
            text(ui);
            preset::button(ui, m, id(VIEW, i), Action::new(VIEW, i as u32), view, ButtonKind::Secondary, false);
            if !below {
                others(ui, m);
            }
        })
        .width(Length::Pct(1.0))
        .height(if below { Length::Pt(control::BUTTON_H) } else { Length::Px(height) })
        .gap(gap)
        .align_cross(AlignCross::Center);
        if below {
            ui.hstack(|ui| others(ui, m)).gap(gap);
        }
    })
    .width(Length::Pct(1.0))
    .height(Length::Px(height))
    .padding(pad)
    .gap(Length::Pt(card::ROW_PAD_Y))
    .whole();
}

/// What the panel was last described from.
struct Described {
    groups: Rc<Vec<HelperGroup>>,
    size: (f64, f64),
    backdrop_top: f64,
    /// The minute, all of the time that shows.
    minute: u64,
}

/// The helper panel's surface.
#[derive(Default)]
pub struct HelpersSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
}

impl HelpersSurface {
    /// The rows the surface was last described from, which a press on it
    /// is about.
    pub fn groups(&self) -> Rc<Vec<HelperGroup>> {
        self.described.as_ref().map(|d| Rc::clone(&d.groups)).unwrap_or_default()
    }

    fn place(&mut self, hp: &HelpersRender, backdrop_top: f64) -> Place {
        let size = (hp.rect.w, hp.rect.h);
        let minute = hp.now_unix / 60;
        let same = self.described.as_ref().is_some_and(|d| {
            (Rc::ptr_eq(&d.groups, &hp.groups) || d.groups == hp.groups)
                && d.size == size
                && d.backdrop_top == backdrop_top
                && d.minute == minute
        });
        if !same {
            self.described = Some(Described { groups: hp.groups.clone(), size, backdrop_top, minute });
            self.data_rev += 1;
        }
        self.kept.surface.interaction.set_scroll(SCROLL as usize, hp.scroll);
        Place { data_rev: self.data_rev, origin: (hp.rect.x, hp.rect.y_top), avail: size }
    }

    /// Bring the tree and its layout up to `hp` without painting.
    pub fn lay_out(&mut self, font: &mut FontCache, hp: &HelpersRender, backdrop_top: f64) {
        let at = self.place(hp, backdrop_top);
        self.kept.lay_out(font, at, |ui, fm| describe(ui, fm, at.avail, backdrop_top, &hp.groups, hp.now_unix));
    }

    /// Add the panel to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, hp: &HelpersRender, backdrop_top: f64) {
        let at = self.place(hp, backdrop_top);
        self.kept.frame(p, at, hp.pointer, |ui, fm| describe(ui, fm, at.avail, backdrop_top, &hp.groups, hp.now_unix));
    }
}
