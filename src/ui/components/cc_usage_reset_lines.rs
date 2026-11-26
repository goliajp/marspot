//! An account card's lines of stored resets, under its bars.  A line
//! marspot can claim for is a button; the one armed by a first press is
//! drawn in the warning ink and says what a second press does.

use crate::agent_resets::{MAX_LINES, ResetLine};
use crate::ui::components::cc_usage_layout::{CardRows, Cell};
use crate::ui::components::cc_usage_modal::cut_to_columns;
use crate::ui::components::cc_usage_view::placed;
use crate::ui::tree::Tone;
use golia_ui_core::{Action, Align, Length, TextFont, Ui, WidgetId};

/// The action of a reset line; its argument is `card * MAX_LINES + line`.
pub const RESET_LINE: u32 = 9;
const RESET_ID: u32 = 30_000;

/// The lines of card `card`, starting at row `first` of `rows`.
pub(super) fn reset_lines(ui: &mut Ui<'_>, lines: &[ResetLine], (card, rows, first, cell): (usize, &CardRows, usize, Cell), scratch: &mut String) {
    for (i, l) in lines.iter().enumerate().take(MAX_LINES) {
        let row = rows.bar_row(first + i, 0, cell);
        scratch.clear();
        scratch.push_str(&l.text);
        cut_to_columns(scratch, (rows.right - rows.left) / cell.w);
        let ink = if l.armed { Tone::WarnText } else { Tone::FgMuted };
        let arg = (card * MAX_LINES + i) as u32;
        placed(ui, (rows.left, row.top), |ui| {
            let t = ui.text_copy(TextFont::Mono, ink.into(), scratch).height(Length::Px(row.h)).align(Align::Leading);
            if l.pressable {
                t.pressable(WidgetId(RESET_ID + arg), Action::new(RESET_LINE, arg));
            }
        });
    }
}
