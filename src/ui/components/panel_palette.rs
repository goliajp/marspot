//! The palette every panel draws from.
//!
//! It started life as the `Cc` panel's private colours and was named
//! for it; the settings panel then grew a second, near-identical set
//! under its own name, and the two drifted — one drew its cards a
//! level *above* the panel, the other a level *below*, so the same
//! object looked raised in one panel and inset in the next.  One
//! module now, named for what it is.
//!
//! Every colour is a theme token.  The only local decisions are which
//! token plays which role.

use crate::ui::theme::spec;
use crate::ui::theme::token::color;
/// Primary — account names, headings, percentages.
pub fn fg() -> [f32; 4] { color::FG.to_rgba_f32() }
/// Secondary — email, reset times, timeline tags.  Deliberately
/// NOT `FG_MUTED`: at this density the muted token collapses the
/// whole panel into one grey and the reader can't find the row
/// they want.  This sits between FG and FG_MUTED so the hierarchy
/// (primary → secondary → axis) stays legible.
pub fn fg_sec() -> [f32; 4] { [0.70, 0.75, 0.82, 1.0] }
/// Tertiary — date axis, tick marks.  Chart furniture, reads as
/// background once you've found your row.
pub fn fg_faint() -> [f32; 4] { color::FG_MUTED.to_rgba_f32() }
/// A line that must recede behind the one above it — an
/// explanation, not a label.  Same token as `fg_faint`; the two
/// names are kept apart because one means "this control is off"
/// and the other means "this is secondary text", and they will not
/// always want the same colour.
pub fn fg_muted() -> [f32; 4] { color::FG_MUTED.to_rgba_f32() }
pub fn ok() -> [f32; 4] { color::SUCCESS.to_rgba_f32() }
pub fn warn() -> [f32; 4] { color::WARN.to_rgba_f32() }
pub fn danger() -> [f32; 4] { color::DANGER.to_rgba_f32() }
/// Unfilled bar remainder.  `SURFACE_4`, not `BG_HOVER` — the
/// track has to separate from the card it sits on (`SURFACE_1`),
/// otherwise "0 %" and "no data" look identical.
pub fn track() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
/// A card sits **on** its panel, so it takes the surface level
/// above the panel's own — the same step in every panel.
pub fn card_bg() -> [f32; 4] { color::SURFACE_3.to_rgba_f32() }
pub fn card_border() -> [f32; 4] { color::BORDER.to_rgba_f32() }
/// Between two rows of one card.  `HAIRLINE`, not `DIVIDER`: the
/// fainter token at one device pixel is invisible, which leaves
/// the rows looking exactly as undivided as no line at all.
pub fn separator() -> [f32; 4] { color::HAIRLINE.to_rgba_f32() }
/// An unpicked segment of a segmented control — one level above
/// the card it sits on.
pub fn segment_bg() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
pub fn now() -> [f32; 4] { color::ACCENT.to_rgba_f32() }
/// Day rules running up through the plot.  Low alpha rather than a
/// dim solid colour so the rule reads as behind the bars on both
/// the card surface and the modal ground it crosses.
///
/// 0.14 is chosen against alpha that actually works: the first cut
/// used 0.22, picked by eye while the ui_rect pipeline was still
/// applying alpha twice, so what was really on screen was 0.05.
pub fn grid() -> [f32; 4] { [0.62, 0.66, 0.74, 0.14] }
/// Status chip fill — the severity colour at low alpha, so the
/// chip reads as tinted glass over the card rather than a second
/// solid block competing with the bars.
pub fn chip_bg(severity: u8) -> [f32; 4] {
    let mut c = severity_color(severity);
    c[3] = spec::control::CHIP_FILL_ALPHA;
    c
}

/// Status severity → colour.  0 = fine, 1 = approaching the cap,
/// 2 = refused / unknown.
pub fn severity_color(severity: u8) -> [f32; 4] {
    match severity {
        0 => ok(),
        1 => warn(),
        _ => danger(),
    }
}

/// No colour at all — the border of something that has none.
pub fn none() -> [f32; 4] { [0.0; 4] }
/// `c` at opacity `alpha`.
pub fn with_alpha(mut c: [f32; 4], alpha: f32) -> [f32; 4] {
    c[3] = alpha;
    c
}
/// A panel's own surface and the line around it.
pub fn frame_bg() -> [f32; 4] { color::BG_PANEL.to_rgba_f32() }
pub fn frame_border() -> [f32; 4] { color::BORDER_STRONG.to_rgba_f32() }
/// What the rest of the window is covered with while a panel is open.
pub fn backdrop() -> [f32; 4] {
    with_alpha(color::SHADOW.to_rgba_f32(), spec::panel::BACKDROP_ALPHA)
}
/// Selected, on, primary.  The one colour that means "this one";
/// `ok` / `warn` / `danger` say how something is doing and are never
/// used for it.
pub fn accent() -> [f32; 4] { color::BG_SELECTED.to_rgba_f32() }
pub fn accent_hover() -> [f32; 4] { hover(accent()) }
/// A fill under the pointer: the same colour, a step towards white.
pub fn hover(c: [f32; 4]) -> [f32; 4] {
    let k = spec::control::HOVER_LIGHTEN;
    [c[0] + (1.0 - c[0]) * k, c[1] + (1.0 - c[1]) * k, c[2] + (1.0 - c[2]) * k, c[3]]
}
/// The label of a control that cannot be used.
pub fn disabled_fg() -> [f32; 4] { color::FG_DISABLED.to_rgba_f32() }
/// A whole control that cannot be used, where it is drawn rather than
/// written: the same colours at reduced opacity.
pub fn faded(c: [f32; 4]) -> [f32; 4] {
    with_alpha(c, c[3] * spec::control::DISABLED_ALPHA)
}
/// Behind the header's close button while the pointer is on it.
pub fn close_hover_bg() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
/// A table row under the pointer, and the selected one.  A row lies
/// on a card, so its hover is the step above the card: the general
/// hover token is darker than a card and vanished on one.
pub fn row_hover() -> [f32; 4] { color::SURFACE_4.to_rgba_f32() }
pub fn row_selected() -> [f32; 4] { with_alpha(accent(), spec::table::SELECTED_ALPHA) }
/// Behind the × of a danger icon button at rest.
pub fn danger_rest_bg() -> [f32; 4] { with_alpha(danger(), spec::control::DANGER_REST_ALPHA) }
