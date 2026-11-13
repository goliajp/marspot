//! The panel every surface of that kind is: the dimmed window, the
//! frame, the header with its title and close button, and the content
//! area under it.
//!
//! The sizes are `ui::theme::spec`'s and the frame is painted by the
//! shared chrome, so a panel described here stands where
//! `PanelChrome` puts one.

use super::{Tone, font_of, preset};
use crate::ui::theme::spec::{self, content, header};
use golia_ui_core::{Action, AlignCross, Edges, Length, Ui, WidgetId};

/// The close button of whatever panel a surface holds.
pub const CLOSE_ID: WidgetId = WidgetId(u32::MAX - 1);
/// The header, where it is the handle the panel is dragged by.
pub const HEADER_ID: WidgetId = WidgetId(u32::MAX - 2);
/// What a press on the close button asks for.
pub const CLOSE: Action = Action::new(u32::MAX - 1, 0);
/// What a press on the header begins.
pub const DRAG: Action = Action::new(u32::MAX - 2, 0);

/// A panel `w × h` pixels: the root of its surface.  `backdrop_top` is
/// the y the window is dimmed from.  `body` describes what goes in the
/// content area, top to bottom.
pub fn panel(
    ui: &mut Ui<'_>,
    (w, h): (f64, f64),
    backdrop_top: f64,
    title: &str,
    meta: Option<&str>,
    body: impl FnOnce(&mut Ui<'_>),
) {
    let pad_x = Length::Pt(content::PAD_X);
    let none = Length::ZERO;
    ui.vstack(|ui| {
        ui.hstack(|ui| {
            ui.text_copy(font_of(header::TITLE), Tone::Fg.into(), title);
            ui.spacer();
            if let Some(meta) = meta {
                ui.text_copy(font_of(header::META), Tone::FgMuted.into(), meta);
            }
            preset::close_button(ui, CLOSE_ID, CLOSE);
        })
        .width(Length::Pct(1.0))
        .height(Length::Px(spec::px(header::H).min(h)))
        .padding(Edges { top: none, right: Length::Pt(header::CLOSE_PAD_RIGHT), bottom: none, left: pad_x })
        .gap(Length::Pt(header::META_GAP))
        .align_cross(AlignCross::Center)
        .pressable(HEADER_ID, DRAG);
        ui.vstack(body)
            .width(Length::Pct(1.0))
            .grow(1)
            .padding(Edges { top: none, right: pad_x, bottom: Length::Pt(content::PAD_BOTTOM), left: pad_x });
    })
    .width(Length::Px(w))
    .height(Length::Px(h))
    .leaf(preset::FRAME, [(backdrop_top as f32).to_bits(), 0]);
}
