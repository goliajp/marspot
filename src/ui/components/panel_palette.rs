//! The palette every panel and the main window draw from.
//!
//! It started life as the `Cc` panel's private colours and was named
//! for it; the settings panel then grew a second, near-identical set
//! under its own name, and the two drifted — one drew its cards a
//! level *above* the panel, the other a level *below*, so the same
//! object looked raised in one panel and inset in the next.  One
//! module now, named for what it is.
//!
//! Every colour is the current theme's.  The only local decisions are
//! which entry of the table plays which role; a role is asked for each
//! time it is drawn, so a change of theme is seen by the next frame.

use crate::ui::theme::spec;
use crate::ui::theme::table::rgba;
/// Primary — account names, headings, percentages.
pub fn fg() -> [f32; 4] { rgba::fg() }
/// Secondary — email, reset times, timeline tags.  Deliberately
/// NOT the muted tone: at this density it collapses the whole panel
/// into one grey and the reader can't find the row they want.  This
/// sits between the two so the hierarchy (primary → secondary → axis)
/// stays legible.
pub fn fg_sec() -> [f32; 4] { rgba::fg_secondary() }
/// Tertiary — date axis, tick marks.  Chart furniture, reads as
/// background once you've found your row.
pub fn fg_faint() -> [f32; 4] { rgba::fg_muted() }
/// A line that must recede behind the one above it — an
/// explanation, not a label.  Same token as `fg_faint`; the two
/// names are kept apart because one means "this control is off"
/// and the other means "this is secondary text", and they will not
/// always want the same colour.
pub fn fg_muted() -> [f32; 4] { rgba::fg_muted() }
pub fn ok() -> [f32; 4] { rgba::success() }
pub fn warn() -> [f32; 4] { rgba::warn() }
pub fn danger() -> [f32; 4] { rgba::danger() }
/// Unfilled bar remainder.  `SURFACE_4`, not `BG_HOVER` — the
/// track has to separate from the card it sits on (`SURFACE_1`),
/// otherwise "0 %" and "no data" look identical.
pub fn track() -> [f32; 4] { rgba::surface_4() }
/// A card sits **on** its panel, so it takes the surface level
/// above the panel's own — the same step in every panel.
pub fn card_bg() -> [f32; 4] { rgba::surface_3() }
pub fn card_border() -> [f32; 4] { rgba::border() }
/// Between two rows of one card.  `HAIRLINE`, not `DIVIDER`: the
/// fainter token at one device pixel is invisible, which leaves
/// the rows looking exactly as undivided as no line at all.
pub fn separator() -> [f32; 4] { rgba::hairline() }
/// An unpicked segment of a segmented control — one level above
/// the card it sits on.
pub fn segment_bg() -> [f32; 4] { rgba::surface_4() }
/// A key cap — the same step above the card as an unpicked segment.
pub fn keycap_bg() -> [f32; 4] { rgba::surface_4() }
pub fn now() -> [f32; 4] { rgba::accent() }
/// Day rules running up through the plot.  Low alpha rather than a
/// dim solid colour so the rule reads as behind the bars on both
/// the card surface and the modal ground it crosses.
///
/// The dark theme's 0.14 is chosen against alpha that actually works:
/// the first cut used 0.22, picked by eye while the ui_rect pipeline
/// was still applying alpha twice, so what was really on screen was
/// 0.05.
pub fn grid() -> [f32; 4] { rgba::grid() }
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

/// Severity as text: a chip's label, a percentage, a mark on the
/// severity's own tint.  `severity_color` is the fill.
pub fn severity_text(severity: u8) -> [f32; 4] {
    match severity {
        0 => rgba::success_text(),
        1 => rgba::warn_text(),
        _ => rgba::danger_text(),
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
pub fn frame_bg() -> [f32; 4] { rgba::bg_panel() }
pub fn frame_border() -> [f32; 4] { rgba::border_strong() }
/// The shadow under a panel's frame, and under a popover's.
pub fn frame_shadow() -> [f32; 4] { shadow(spec::panel::SHADOW_ALPHA) }
/// The theme's shadow colour at opacity `alpha`.
pub fn shadow(alpha: f32) -> [f32; 4] { with_alpha(rgba::shadow(), alpha) }
/// What the rest of the window is covered with while a panel is open.
pub fn backdrop() -> [f32; 4] {
    with_alpha(rgba::backdrop(), spec::panel::BACKDROP_ALPHA)
}
/// Selected, on, primary.  The one colour that means "this one";
/// `ok` / `warn` / `danger` say how something is doing and are never
/// used for it.
pub fn accent() -> [f32; 4] { rgba::bg_selected() }
pub fn accent_hover() -> [f32; 4] { hover(accent()) }
/// A label or a mark on the accent, or on a status colour used as a
/// fill.
pub fn on_accent() -> [f32; 4] { rgba::fg_on_accent() }
/// A fill under the pointer: the same colour, a step towards white on
/// a dark theme and towards black on a light one.
pub fn hover(c: [f32; 4]) -> [f32; 4] {
    let k = spec::control::HOVER_LIGHTEN;
    let t = rgba::hover_tint();
    [c[0] + (t[0] - c[0]) * k, c[1] + (t[1] - c[1]) * k, c[2] + (t[2] - c[2]) * k, c[3]]
}
/// A toggle that is off, and the knob of one in either state.
pub fn toggle_off() -> [f32; 4] { rgba::toggle_off() }
pub fn knob() -> [f32; 4] { rgba::knob() }
/// The label of a control that cannot be used.
pub fn disabled_fg() -> [f32; 4] { rgba::fg_disabled() }
/// A whole control that cannot be used, where it is drawn rather than
/// written: the same colours at reduced opacity.
pub fn faded(c: [f32; 4]) -> [f32; 4] {
    with_alpha(c, c[3] * spec::control::DISABLED_ALPHA)
}
/// Behind the header's close button while the pointer is on it.
pub fn close_hover_bg() -> [f32; 4] { rgba::surface_4() }
/// A table row under the pointer, and the selected one.  A row lies
/// on a card, so its hover is the step above the card: the general
/// hover token is darker than a card and vanished on one.
pub fn row_hover() -> [f32; 4] { rgba::surface_4() }
pub fn row_selected() -> [f32; 4] { with_alpha(accent(), spec::table::SELECTED_ALPHA) }
/// Behind the × of a danger icon button at rest.
pub fn danger_rest_bg() -> [f32; 4] { with_alpha(danger(), spec::control::DANGER_REST_ALPHA) }

// ─── Main window ────────────────────────────────────────────────
//
// The window's own surface is darker than a panel's, so what lies on
// it takes the general hover token where a card takes the step above.

/// The pane in use is the terminal's own black; the others sit at the
/// window's surface, with the sidebar and the header.
pub fn pane_bg(in_use: bool) -> [f32; 4] {
    if in_use { rgba::canvas() } else { rgba::chrome() }
}
/// The line between two regions of the window: pane and pane, sidebar
/// and grid, header and grid, title strip and terminal.
pub fn seam() -> [f32; 4] { rgba::seam() }
/// Around the pane in use, in place of its seams.
pub fn focus_ring() -> [f32; 4] { rgba::focus_ring() }
/// A pane's name where it is not the one in use — a sidebar row, a
/// title strip.  The one in use takes `fg`.
pub fn chrome_fg() -> [f32; 4] { fg_sec() }
/// A toolbar icon or a ghost button's glyph at rest, and the version
/// label: there, readable, not asking to be read.
pub fn chrome_icon() -> [f32; 4] { rgba::fg_muted() }
/// Behind a toolbar button, a sidebar row or a ghost button under the
/// pointer.
pub fn chrome_hover() -> [f32; 4] { rgba::bg_hover() }
/// Behind the sidebar row of the pane in use and a toolbar button
/// whose panel is open: the accent, tinted, as on a selected table row.
pub fn chrome_on() -> [f32; 4] { row_selected() }
/// An agent's badge at the right of a pane's title strip.
pub fn badge_fg() -> [f32; 4] { rgba::agent() }
/// Over a seat nothing runs in, and the `+` drawn on it.
pub fn empty_seat_bg() -> [f32; 4] { with_alpha(rgba::chrome(), spec::chrome::EMPTY_SEAT_ALPHA) }
pub fn empty_seat_fg() -> [f32; 4] { faded(disabled_fg()) }
/// What dims a pane that has stepped back, at opacity `alpha`.
pub fn scrim(alpha: f32) -> [f32; 4] { with_alpha(rgba::scrim(), alpha) }
/// Where a dragged pane would land: the accent as the outline, and
/// inside it at low opacity when the drop takes half a pane.
pub fn drop_target_border() -> [f32; 4] { rgba::accent() }
pub fn drop_target_fill() -> [f32; 4] {
    let k = spec::chrome::DROP_TARGET_ALPHA;
    let c = drop_target_border();
    [c[0] * k, c[1] * k, c[2] * k, k]
}

// ─── Presets and the shell ──────────────────────────────────────

/// The lowest surface.
pub fn surface_base() -> [f32; 4] { rgba::bg() }
/// The shell's status banner: its plate and its text.
pub fn notice_bg() -> [f32; 4] { rgba::notice_bg() }
pub fn notice_fg() -> [f32; 4] { rgba::notice_fg() }
