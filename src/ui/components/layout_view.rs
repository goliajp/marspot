//! The layout panel as a tree: the Grid group with a stepper per
//! axis, the Arrangement group with a card per slot, and the footer
//! with the pane count and Apply.
//!
//! How big the panel is, how far its cards shrink in a short window
//! and where a dragged card would land are `LayoutModal`'s; this file
//! describes a tree that fills the frame it gives.  A card's label is
//! cut to its card when the tree is described, so the node carries the
//! string that is drawn and it is shaped as one run.
//!
//! The card being dragged floats: it follows the pointer anywhere in
//! the window, over the panel and outside it, above everything else
//! the surface draws.

use std::cell::RefCell;
use std::rc::Rc;

use crate::font_cache::FontCache;
use crate::ui::components::controls::{self, ButtonKind, Measure, StepperPart};
use crate::ui::components::layout_modal::{self as lm, Axis, Hover, LayoutModal, metric, GRID_MAX, GRID_MIN};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{self, t, Arg};
use crate::ui::theme::spec::{self, card, content, space};
use crate::ui::tree::kept::Place;
use crate::ui::tree::{FontMeasure, KeptSurface, Tone, font_of, panel, preset};
use golia_ui_core::{
    Action, Align, AlignCross, Edges, FontSpec, Length, TextMeasure, Ui, WidgetId,
};
use marspot_term::layout::Rect;

/// A stepper button was pressed: the argument is [`step_arg`].
pub const STEP: u32 = 2;
/// Apply was pressed.
pub const APPLY: u32 = 3;
/// A pane card was pressed: the argument is its slot.
pub const CARD: u32 = 4;

const STEP_ID: u32 = 100;
const APPLY_ID: WidgetId = WidgetId(110);
const CARD_ID: u32 = 1000;

fn step_arg(axis: Axis, part: StepperPart) -> u32 {
    ((axis == Axis::Rows) as u32) << 1 | (part == StepperPart::Plus) as u32
}

/// What an action of this panel asks for, as the panel names it.
pub fn asked(action: Action) -> Hover {
    match action.id {
        STEP => {
            let axis = if action.arg & 2 != 0 { Axis::Rows } else { Axis::Columns };
            let part = if action.arg & 1 != 0 { StepperPart::Plus } else { StepperPart::Minus };
            Hover::Step(axis, part)
        }
        APPLY => Hover::Apply,
        CARD => Hover::Card(action.arg as usize),
        _ if action == panel::CLOSE => Hover::Close,
        _ => Hover::None,
    }
}

/// One frame of the layout panel: the pending grid shape, what each
/// slot holds, what the pointer is doing, and the surface it is drawn
/// through.
#[derive(Clone, Default)]
pub struct LayoutModalRender {
    pub cols: usize,
    pub rows: usize,
    /// One title per slot, in slot order (length = cols * rows).
    /// Empty string = empty slot (no card content drawn).
    pub slot_titles: Vec<String>,
    /// Drag state if a card drag is in progress this frame.
    pub drag: Option<LayoutModalDragRender>,
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    /// How far the panel's header has been dragged from the middle.
    pub offset: (f64, f64),
    pub surface: Rc<RefCell<LayoutSurface>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutModalDragRender {
    pub from_slot: usize,
    pub grab_offset_phys: (f64, f64),
    pub mouse_phys: (f64, f64),
}

/// How a pane card is drawn.
#[derive(Clone, Copy, PartialEq)]
enum CardState {
    Rest,
    /// The slot the dragged card came from: a hole.
    Origin,
    /// The slot it would land in.
    Target,
    /// The card under the pointer: a level above the cards it passes
    /// over, outlined so its edge stays visible against them.
    Lifted,
}

/// Where the label of the card at `rect` is drawn, as `(x, top, w)`,
/// for telling whether the dragged card is over it.
fn label_box(fm: &FontMeasure<'_>, rect: Rect, fitted: &str) -> (f64, f64, f64) {
    let role = metric::CARD_LABEL;
    let font = FontSpec::new(role.pt(), role.weight());
    let w = fm.advance(fitted, font);
    let by = fm.centring(fitted, font);
    let baseline = rect.y_top as f32 + ((rect.h as f32 + by.above as f32 - by.below as f32) * 0.5).round();
    (rect.x + (rect.w - w) * 0.5, baseline as f64 - spec::px(role.cap()), w)
}

/// The panel for `state`, filling the frame `modal` gives it.
/// `scratch` holds a label while it is being cut.
pub fn describe(
    ui: &mut Ui<'_>,
    fm: &FontMeasure<'_>,
    state: &LayoutModalRender,
    modal: &LayoutModal,
    backdrop_top: f64,
    scratch: &mut String,
) {
    let mut m = fm.for_controls();
    let frame = modal.chrome.frame;
    let (cols, rows) = (state.cols.max(1), state.rows.max(1));
    let card_size = modal.cards.first().map_or((0.0, 0.0), |c| (c.w, c.h));
    // the gap between two rows of cards shrinks with them
    let gap_y = modal.cards.get(cols).map_or(0.0, |below| below.y_top - modal.cards[0].y_top - card_size.1);
    let origin = state.drag.map(|d| d.from_slot);
    let target = state.drag.and_then(|d| modal.drop_target(d.from_slot, d.mouse_phys, d.grab_offset_phys));
    let lifted = lifted_rect(modal, state);

    panel::panel(ui, (frame.w, frame.h), backdrop_top, t(lm::TITLE), None, |ui| {
        ui.text(font_of(content::HEADING), Tone::Fg.into(), t(lm::GRID_HEADING));
        ui.leaf(0, Length::ZERO, Length::Pt(space::HEADING_TO_CARD));
        ui.vstack(|ui| {
            for (n, (axis, value)) in [(Axis::Columns, state.cols), (Axis::Rows, state.rows)].into_iter().enumerate() {
                if n > 0 {
                    ui.rule(Length::Px(card::SEPARATOR_PX), Tone::Separator.into())
                        .padding(Edges::xy(Length::Pt(card::SEPARATOR_INSET), Length::ZERO));
                }
                ui.hstack(|ui| {
                    ui.text(font_of(metric::ROW_LABEL), Tone::Fg.into(), t(axis.label()));
                    ui.spacer();
                    let end = |part: StepperPart, enabled: bool| preset::Step {
                        id: WidgetId(STEP_ID + step_arg(axis, part)),
                        action: Action::new(STEP, step_arg(axis, part)),
                        enabled,
                    };
                    let mut digits = [0u8; 20];
                    preset::stepper(
                        ui,
                        end(StepperPart::Minus, value > GRID_MIN),
                        decimal(value, &mut digits),
                        end(StepperPart::Plus, value < GRID_MAX),
                    );
                })
                .width(Length::Pct(1.0))
                .height(Length::Pt(metric::ROW_H))
                .padding(Edges::xy(Length::Pt(card::ROW_PAD_X), Length::ZERO))
                .align_cross(AlignCross::Center);
            }
        })
        .width(Length::Pct(1.0))
        .fill(Tone::CardBg.into())
        .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
        .radius(Length::Pt(card::RADIUS));

        ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_HEADING));
        ui.text(font_of(content::HEADING), Tone::Fg.into(), t(lm::ARRANGEMENT_HEADING));
        ui.leaf(0, Length::ZERO, Length::Pt(space::HEADING_TO_CARD));
        ui.vstack(|ui| {
            for row in 0..rows {
                ui.hstack(|ui| {
                    for col in 0..cols {
                        let slot = row * cols + col;
                        let state_of = if origin == Some(slot) {
                            CardState::Origin
                        } else if target == Some(slot) {
                            CardState::Target
                        } else {
                            CardState::Rest
                        };
                        let title = state.slot_titles.get(slot).map_or("", String::as_str);
                        pane_card(ui, fm, &mut m, slot, modal.cards[slot], state_of, title, lifted, scratch);
                    }
                })
                .gap(Length::Pt(metric::CARD_GAP));
            }
        })
        .gap(Length::Px(gap_y));

        // pinned to the bottom of the content area whatever is above
        // it: in a window too short for the two groups it stands over
        // them rather than leave the frame
        let footer = modal.footer;
        ui.float(footer.x - frame.x, footer.y_top - frame.y_top, |ui| {
            ui.hstack(|ui| {
            ui.text_with(font_of(metric::CAPTION), Tone::FgMuted.into(), |out| {
                // writing into a `String` cannot fail
                let _ = strings::write(out, Arg::PaneCount { cols: state.cols, rows: state.rows });
            });
            ui.spacer();
            preset::button(ui, &mut m, APPLY_ID, Action::new(APPLY, 0), t(lm::APPLY), ButtonKind::Primary, false);
            })
            .width(Length::Px(footer.w))
            .height(Length::Px(footer.h))
            .align_cross(AlignCross::Center);
        });

        if let (Some(d), Some(rect)) = (state.drag, lifted) {
            let title = state.slot_titles.get(d.from_slot).map_or("", String::as_str);
            ui.float(rect.x - frame.x, rect.y_top - frame.y_top, |ui| {
                pane_card(ui, fm, &mut m, d.from_slot, rect, CardState::Lifted, title, None, scratch);
            });
        }
    });
}

/// `n` in decimal, written into `buf`.
fn decimal(mut n: usize, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).expect("digits")
}

/// One slot's card: its fill, and its pane's name in the middle, cut
/// to the card.  The name is left out where the card being dragged
/// (`lifted`) is over it: text is drawn above every rectangle, so it
/// would show through.
#[allow(clippy::too_many_arguments)]
fn pane_card(
    ui: &mut Ui<'_>,
    fm: &FontMeasure<'_>,
    m: Measure<'_>,
    slot: usize,
    rect: Rect,
    state: CardState,
    title: &str,
    lifted: Option<Rect>,
    scratch: &mut String,
) {
    let role = metric::CARD_LABEL;
    let line_h = spec::px(role.cap() + role.descent());
    let (fill, border, fg) = match state {
        CardState::Rest => (Tone::CardBg, Tone::CardBorder, Tone::Fg),
        CardState::Origin => (Tone::FrameBg, Tone::CardBorder, Tone::FgFaint),
        CardState::Target => (Tone::Accent, Tone::Accent, Tone::Fg),
        CardState::Lifted => (Tone::SegmentBg, Tone::FgMuted, Tone::Fg),
    };
    // a card squeezed thinner than a line of text holds none
    let mut labelled = !title.is_empty() && rect.h >= line_h;
    if labelled {
        lm::fit_label_into(title, lm::label_room(rect.w), m, scratch);
        if let Some(l) = lifted {
            let (x, top, w) = label_box(fm, rect, scratch);
            labelled = !(x < l.x + l.w && l.x < x + w && top < l.y_top + l.h && l.y_top < top + line_h);
        }
    }
    let (w, h) = (Length::Px(rect.w), Length::Px(rect.h));
    let node = if labelled {
        ui.text_copy(font_of(role), fg.into(), scratch).by_ink().width(w).height(h).align(Align::Center)
    } else {
        ui.leaf(0, w, h)
    };
    let node = node
        .fill(fill.into())
        .border(Length::Px(card::BORDER_PX as f64), border.into())
        .radius(Length::Pt(card::RADIUS));
    // the card in the hand answers nothing: what is under it does
    if state != CardState::Lifted {
        node.pressable(WidgetId(CARD_ID + slot as u32), Action::new(CARD, slot as u32));
    }
}

fn lifted_rect(modal: &LayoutModal, state: &LayoutModalRender) -> Option<Rect> {
    state.drag.and_then(|d| modal.lifted_card(d.from_slot, d.mouse_phys, d.grab_offset_phys))
}

/// What the panel was last described from.
#[derive(PartialEq)]
struct Described {
    cols: usize,
    rows: usize,
    titles: Vec<String>,
    drag: Option<LayoutModalDragRender>,
    frame: Rect,
    backdrop_top: f64,
}

/// The layout panel's surface.
#[derive(Default)]
pub struct LayoutSurface {
    pub kept: KeptSurface,
    described: Option<Described>,
    data_rev: u64,
    scratch: String,
}

impl LayoutSurface {
    /// Where `state` puts the panel in a window `window` pixels in
    /// size, having counted a change in what it shows.
    fn place(
        &mut self,
        font: &mut FontCache,
        state: &LayoutModalRender,
        window: (f64, f64),
        top_inset: f64,
    ) -> (LayoutModal, Place) {
        // measured the way the click handler measures, so both arrive
        // at the same panel
        let modal = {
            let mut m = controls::font_measure(font);
            let label_w = lm::widest_label(&state.slot_titles, &mut m);
            LayoutModal::layout_moved(
                window.0, window.1, top_inset, state.cols, state.rows, label_w, &mut m, state.offset,
            )
        };
        let frame = modal.chrome.frame;
        let same = self.described.as_ref().is_some_and(|d| {
            d.cols == state.cols
                && d.rows == state.rows
                && d.drag == state.drag
                && d.frame == frame
                && d.backdrop_top == top_inset
                && d.titles == state.slot_titles
        });
        if !same {
            // the old value's strings are reused where there are as many
            let mut titles = self.described.take().map(|d| d.titles).unwrap_or_default();
            titles.resize(state.slot_titles.len(), String::new());
            for (kept, now) in titles.iter_mut().zip(&state.slot_titles) {
                kept.clone_from(now);
            }
            self.described = Some(Described {
                cols: state.cols,
                rows: state.rows,
                titles,
                drag: state.drag,
                frame,
                backdrop_top: top_inset,
            });
            self.data_rev += 1;
        }
        let at = Place { data_rev: self.data_rev, origin: (frame.x, frame.y_top), avail: (frame.w, frame.h) };
        (modal, at)
    }

    /// Bring the tree and its layout up to `state` without painting.
    pub fn lay_out(&mut self, font: &mut FontCache, state: &LayoutModalRender, window: (f64, f64), top_inset: f64) {
        let (modal, at) = self.place(font, state, window, top_inset);
        let scratch = &mut self.scratch;
        self.kept.lay_out(font, at, |ui, fm| describe(ui, fm, state, &modal, top_inset, scratch));
    }

    /// Add the panel to the frame `p` is painting.
    pub fn frame(&mut self, p: &mut ViewPainter<'_>, state: &LayoutModalRender, top_inset: f64) {
        let (modal, at) = self.place(&mut *p.font, state, (p.window_w, p.window_h), top_inset);
        let scratch = &mut self.scratch;
        self.kept.frame(p, at, state.pointer, |ui, fm| describe(ui, fm, state, &modal, top_inset, scratch));
    }
}
