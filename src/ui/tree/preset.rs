//! The shared controls as nodes of a tree.
//!
//! A preset does not draw a control a second way.  Its size is what
//! the control's geometry function says, and it is painted by the
//! control's paint function, handed the rectangle layout gave the
//! node: a control in a tree and the same control painted by hand run
//! the same code and cannot come out differently.

use crate::ui::components::controls::{self, Measure};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{Msg, t};
use crate::ui::theme::spec::control as c;
use golia_ui_core::{Action, GroupKind, Length, Ui, WidgetId, WidgetState};
use marspot_term::layout::Rect;

/// One segment of a segmented control.  `data[0]` is nonzero for the
/// segment the application's data has chosen.
pub const SEGMENT: u32 = 1;

/// A segmented control: one stop for Tab, the arrow keys move along
/// it, and segment `n` emits `Action { id: action, arg: n }`.
///
/// `chosen` is `None` when the value in force is not one of the
/// options.
pub fn segmented(
    ui: &mut Ui<'_>,
    m: Measure<'_>,
    id: WidgetId,
    action: u32,
    labels: &'static [Msg],
    chosen: Option<usize>,
    disabled: bool,
) {
    ui.group(id, GroupKind::Segmented, false, |ui| {
        for (n, label) in labels.iter().enumerate() {
            let picked = chosen == Some(n);
            ui.leaf(SEGMENT, Length::Px(controls::segment_width(*label, m)), Length::Pt(c::SEGMENT_H))
                .leaf(SEGMENT, [picked as u32, 0])
                .label(t(*label))
                .item(Action::new(action, n as u32))
                .current(picked)
                .disabled(disabled);
        }
    })
    .gap(Length::Pt(c::SEGMENT_GAP));
}

/// Paint the preset `tag` in `rect`.
pub(super) fn paint(p: &mut ViewPainter, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState) {
    match tag {
        SEGMENT => controls::paint_segment(p, rect, label, data[0] != 0, state.hovered, state.disabled),
        _ => unreachable!("a node asked for preset {tag}, which nothing here draws"),
    }
}
