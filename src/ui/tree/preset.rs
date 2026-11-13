//! The shared controls as nodes of a tree.
//!
//! A preset does not draw a control a second way.  Its size is what
//! the control's geometry function says, and it is painted by the
//! control's paint function, handed the rectangle layout gave the
//! node: a control in a tree and the same control painted by hand run
//! the same code and cannot come out differently.
//!
//! Whether the pointer is on a control is not a parameter here: the
//! surface knows, and tells the paint function when it paints.

use crate::ui::components::controls::{self, ButtonKind, Measure, StepperPart};
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{Msg, t};
use crate::ui::theme::spec::{self, control as c, header};
use golia_ui_core::{Action, GroupKind, Length, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;

/// One segment of a segmented control.  `data[0]` is nonzero for the
/// segment the application's data has chosen.
pub const SEGMENT: u32 = 1;
/// A text button.  `data[0]` is nonzero for a secondary one.
pub const BUTTON: u32 = 2;
/// A toggle.  `data[0]` is nonzero when it is on.
pub const TOGGLE: u32 = 3;
/// One of a stepper's buttons.  `data[0]` is nonzero for the plus.
pub const STEP: u32 = 4;
/// The number between a stepper's buttons.
pub const STEP_VALUE: u32 = 5;
/// A status chip.  `data[0]` is its severity.
pub const CHIP: u32 = 6;
/// The danger icon button.
pub const DANGER: u32 = 7;
/// A panel's close button.
pub const CLOSE: u32 = 8;
/// A panel's frame and the dimmed window behind it.  `data[0]` is the
/// bits of the y the dimming starts at, as an `f32`.
pub const FRAME: u32 = 9;
/// The key caps of one row of the shortcuts panel: see
/// `shortcuts_view::paint_keys`.
pub const KEYS: u32 = 10;

/// A segmented control whose segment `n` emits
/// `Action { id: action, arg: n }`.
pub fn segmented(
    ui: &mut Ui<'_>,
    m: Measure<'_>,
    id: WidgetId,
    action: u32,
    labels: &'static [Msg],
    chosen: Option<usize>,
    disabled: bool,
) {
    segmented_with(ui, m, id, labels, chosen, disabled, |n| Action::new(action, n as u32));
}

/// A segmented control: one stop for Tab, the arrow keys move along
/// it, and segment `n` emits `action_of(n)`.
///
/// `chosen` is `None` when the value in force is not one of the
/// options.
pub fn segmented_with(
    ui: &mut Ui<'_>,
    m: Measure<'_>,
    id: WidgetId,
    labels: &'static [Msg],
    chosen: Option<usize>,
    disabled: bool,
    action_of: impl Fn(usize) -> Action,
) {
    ui.group(id, GroupKind::Segmented, false, |ui| {
        for (n, label) in labels.iter().enumerate() {
            let picked = chosen == Some(n);
            ui.leaf(SEGMENT, Length::Px(controls::segment_width(*label, m)), Length::Pt(c::SEGMENT_H))
                .leaf(SEGMENT, [picked as u32, 0])
                .label(t(*label))
                .radius(Length::Pt(c::SEGMENT_RADIUS))
                .item(action_of(n))
                .current(picked)
                .disabled(disabled);
        }
    })
    .gap(Length::Pt(c::SEGMENT_GAP))
    .disabled(disabled);
}

/// A text button, as wide as its label asks.
pub fn button(
    ui: &mut Ui<'_>,
    m: Measure<'_>,
    id: WidgetId,
    action: Action,
    label: &str,
    kind: ButtonKind,
    disabled: bool,
) {
    let (w, h) = controls::button_size(label, m);
    ui.leaf(BUTTON, Length::Px(w), Length::Px(h))
        .leaf(BUTTON, [(kind == ButtonKind::Secondary) as u32, 0])
        .label_copy(label)
        .radius(Length::Pt(c::BUTTON_RADIUS))
        .widget(id, action)
        .disabled(disabled);
}

pub fn toggle(ui: &mut Ui<'_>, id: WidgetId, action: Action, on: bool, disabled: bool) {
    ui.leaf(TOGGLE, Length::Pt(c::TOGGLE_W), Length::Pt(c::TOGGLE_H))
        .leaf(TOGGLE, [on as u32, 0])
        .radius(Length::Pt(c::TOGGLE_H * 0.5))
        .widget(id, action)
        .disabled(disabled);
}

/// One end of a stepper: the widget its button is, what pressing it
/// asks for, and whether the value can go further that way.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub id: WidgetId,
    pub action: Action,
    pub enabled: bool,
}

/// A stepper: minus, the value, plus.  Each button is a stop of its
/// own.
pub fn stepper(ui: &mut Ui<'_>, minus: Step, value: &str, plus: Step) {
    let side = Length::Pt(c::STEPPER_BUTTON);
    ui.hstack(|ui| {
        let end = |ui: &mut Ui<'_>, step: Step, part: StepperPart| {
            ui.leaf(STEP, side, side)
                .leaf(STEP, [(part == StepperPart::Plus) as u32, 0])
                .radius(Length::Pt(c::BUTTON_RADIUS))
                .widget(step.id, step.action)
                .disabled(!step.enabled);
        };
        end(ui, minus, StepperPart::Minus);
        ui.leaf(STEP_VALUE, Length::Pt(c::STEPPER_VALUE_W), side).label_copy(value);
        end(ui, plus, StepperPart::Plus);
    });
}

/// A status chip: 0 fine, 1 near a limit, anything else a problem.
pub fn chip(ui: &mut Ui<'_>, m: Measure<'_>, label: &str, severity: u8) {
    let (w, h) = controls::chip_size(label, m);
    ui.leaf(CHIP, Length::Px(w), Length::Px(h)).leaf(CHIP, [severity as u32, 0]).label_copy(label);
}

/// The danger icon button, `side` pixels square.
pub fn danger_button(ui: &mut Ui<'_>, id: WidgetId, action: Action, side: f64) {
    let k = side / spec::px(c::DANGER_SIZE);
    ui.leaf(DANGER, Length::Px(side), Length::Px(side))
        .radius(Length::Pt(c::DANGER_RADIUS * k))
        .widget(id, action);
}

/// A panel's close button.
pub fn close_button(ui: &mut Ui<'_>, id: WidgetId, action: Action) {
    let side = Length::Pt(header::CLOSE_SIZE);
    ui.leaf(CLOSE, side, side).radius(Length::Pt(header::CLOSE_RADIUS)).widget(id, action);
}

/// Paint the preset `tag` in `rect`.
pub(super) fn paint(p: &mut ViewPainter, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState) {
    let on = data[0] != 0;
    match tag {
        SEGMENT => controls::paint_segment(p, rect, label, on, state.hovered, state.disabled),
        BUTTON => {
            let kind = if on { ButtonKind::Secondary } else { ButtonKind::Primary };
            controls::paint_button(p, rect, label, kind, state.hovered, state.disabled)
        }
        TOGGLE => controls::paint_toggle(p, rect, on, state.disabled),
        STEP => {
            let part = if on { StepperPart::Plus } else { StepperPart::Minus };
            controls::paint_stepper_button(p, spec::snap_rect(rect), part, !state.disabled, state.hovered)
        }
        STEP_VALUE => controls::paint_stepper_value(p, spec::snap_rect(rect), label),
        CHIP => controls::paint_chip(p, rect, label, data[0] as u8),
        DANGER => controls::paint_danger_button(p, rect, state.hovered),
        CLOSE => panel_chrome::paint_close(p, rect, state.hovered),
        FRAME => PanelChrome::new(rect, f32::from_bits(data[0]) as f64).frame_view().paint(p, |_| {}),
        KEYS => crate::ui::components::shortcuts_view::paint_keys(p, rect, label, data[0]),
        _ => unreachable!("a node asked for preset {tag}, which nothing here draws"),
    }
}
