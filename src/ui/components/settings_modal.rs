//! Settings panel — where its rows go, and what the pointer is on.
//! The rows themselves are `settings_rows`.
//!
//! Same split as the other modals (`cc_usage_modal` is the reference):
//! shape and behaviour here, painting in `render_metal`.  The layout
//! is a list of cards, each holding rows separated by a hairline —
//! the shape the OS uses for exactly this job, and the shape a flat
//! stack of same-sized lines could never be spaced into.
//!
//! ## The rows are data
//!
//! A row is a label, a **cost**, and a control.  The cost line is not
//! decoration — it is the reason the row exists.  Every entry in this
//! panel is a decision marspot has chosen not to make for the user,
//! which means every entry has a downside worth naming; a settings
//! panel that lists only the upsides is asking the user to choose
//! blind.  Row text lives next to the row's effect for exactly that
//! reason: they cannot drift apart.
//!
//! Anything with a right answer is not here.  The rules that keep
//! reclamation safe — never the focused pane, never with work in
//! flight — are correctness, and a switch for them would only be a
//! way to break them.

use crate::settings::Settings;
pub use crate::ui::components::settings_choices::*;
pub use crate::ui::components::settings_rows::*;

/// What is this panel's own, in **typographic points**.  Everything
/// else -- header, paddings, card, control sizes -- is the shared
/// spec's (`ui::theme::spec`), so this panel cannot drift from the
/// others.
pub mod metric {
    use crate::ui::theme::{spec, PanelText};

    /// Panel width.  Sized for the longest cost line plus the widest
    /// control on the same row, with room to spare.
    pub const PANEL_W: f64 = 480.0;
    /// The title line's height -- that of the tallest control, so
    /// every row in a card is the same height whatever it holds.  A
    /// card whose rows jump between two heights reads as broken.
    pub const ROW_BAND_H: f64 = spec::control::SEGMENT_H;

    pub const LABEL: PanelText = PanelText::Label;
    pub const DESC: PanelText = PanelText::Secondary;
    pub const FOOTER: PanelText = PanelText::Caption;
    /// The text between a picker's buttons.
    pub const PICKER_VALUE: PanelText = PanelText::Label;
    /// Room for a font family's name between its row's buttons; a
    /// longer name is cut short.
    pub const FAMILY_W: f64 = 150.0;

    /// The width of a picker's value: a family name, or a number.
    pub fn picker_value_w_pt(arrows: bool) -> f64 {
        if arrows { FAMILY_W } else { spec::control::STEPPER_VALUE_W }
    }
}

/// A piece of the panel, with where it goes.  The walker emits these
/// in draw order; the painter draws them and the hit-test looks at
/// them, so a control cannot be clickable anywhere but where it is
/// drawn.
pub enum Slot {
    /// A group's heading: its line starts at `top`, at the card's
    /// left edge `x`, over the card's width `w`.
    Group { heading: &'static str, x: f64, top: f64, w: f64 },
    /// A group's card.  Emitted before the rows it contains.
    Card { rect: Rect },
    Row {
        row: Row,
        spec: &'static RowSpec,
        /// The row's full band inside the card — what a hover would
        /// highlight, and what the hairline sits at the bottom of.
        band: Rect,
        label_baseline: f64,
        desc_baseline: f64,
        /// Right-aligned in the card, centred on the title line.
        control: Rect,
    },
    /// Hairline between two rows of `card`, its top edge at `y`.
    Separator { card: Rect, y: f64 },
    Footer { baseline: f64 },
}

use marspot_term::layout::Rect;

use crate::ui::components::controls::{self, Measure};
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::theme::spec::{self, card, content, space};

/// One row's height — constant across a card whatever control it
/// holds, so the card does not step.
pub fn row_h_pt() -> f64 {
    card::ROW_PAD_Y * 2.0
        + metric::ROW_BAND_H
        + space::LABEL_TO_DESC
        + metric::DESC.cap()
        + metric::DESC.descent()
}

/// Walk the panel's content in draw order.  **One walker**, shared by
/// the painter, the hit-test and the height calculation — the
/// alternative is a layout pass plus a matching set of coordinates in
/// the click handler, which is how a control ends up reacting a row
/// above where it is drawn with nothing in a screenshot to say which
/// is wrong.
///
/// `rect` is the panel's frame in physical px.  The header is the
/// shared chrome's and is not walked; everything here lives in the
/// chrome's content rect.
///
/// `scroll` is how far the content has been pushed up under the
/// header.  It goes through the one walker the painter and the
/// hit-test share, so the two cannot disagree about it.
pub fn walk(rect: Rect, scroll: f64, s: &Settings, m: Measure<'_>, mut on: impl FnMut(Slot)) {
    let content = PanelChrome::new(rect, 0.0).content;
    let (card_x, card_w) = (content.x, content.w);
    let row_h = spec::px(row_h_pt());
    let band_h = spec::px(metric::ROW_BAND_H);

    let mut y = content.y_top - scroll;
    for (si, section) in SECTIONS.iter().enumerate() {
        if si > 0 {
            y += spec::px(space::CARD_TO_HEADING);
        }
        on(Slot::Group {
            heading: crate::ui::strings::t(section.heading),
            x: card_x,
            top: y,
            w: card_w,
        });
        y += panel_chrome::heading_height();

        let card = Rect { x: card_x, y_top: y, w: card_w, h: row_h * section.rows.len() as f64 };
        on(Slot::Card { rect: card });

        for (ri, row_spec) in section.rows.iter().enumerate() {
            let row_top = card.y_top + row_h * ri as f64;
            let band_top = row_top + spec::px(card::ROW_PAD_Y);
            let (ctl_w, ctl_h) = control_size(row_spec.row, s, m);
            on(Slot::Row {
                row: row_spec.row,
                spec: row_spec,
                band: Rect { x: card_x, y_top: row_top, w: card_w, h: row_h },
                // Label optically centred in the band, so a label and
                // the control beside it share one centre line.
                label_baseline: band_top + (band_h + spec::px(metric::LABEL.cap())) * 0.5,
                desc_baseline: band_top
                    + band_h
                    + spec::px(space::LABEL_TO_DESC + metric::DESC.cap()),
                control: Rect {
                    x: card_x + card_w - spec::px(card::ROW_PAD_X) - ctl_w,
                    y_top: band_top + (band_h - ctl_h) * 0.5,
                    w: ctl_w,
                    h: ctl_h,
                },
            });
            if ri + 1 < section.rows.len() {
                on(Slot::Separator { card, y: row_top + row_h });
            }
        }
        y = card.y_top + card.h;
    }

    y += spec::px(space::CARD_TO_FOOTER + metric::FOOTER.cap());
    on(Slot::Footer { baseline: y });
}

/// Everything under the header, in physical px, from the same walk
/// that draws it — so "the panel has a dead band at the bottom" is
/// not expressible.
pub fn content_h(s: &Settings, m: Measure<'_>) -> f64 {
    // The walk needs a rect; height does not depend on where it is.
    let probe = Rect { x: 0.0, y_top: 0.0, w: spec::px(metric::PANEL_W), h: 0.0 };
    let top = PanelChrome::new(probe, 0.0).content.y_top;
    let mut bottom = top;
    walk(probe, 0.0, s, m, |slot| {
        if let Slot::Footer { baseline } = slot {
            bottom = baseline;
        }
    });
    bottom - top
}

/// The panel's rect, centred, **sized by what it actually draws**.
pub fn panel_rect(
    w_phys: f64,
    h_phys: f64,
    s: &Settings,
    m: Measure<'_>,
    top_inset: f64,
) -> Rect {
    panel_chrome::frame_rect(
        w_phys,
        h_phys,
        spec::px(metric::PANEL_W),
        panel_chrome::frame_height(content_h(s, m)),
        top_inset,
    )
}

/// The same walk, with everything outside the content area dropped.
///
/// The paint path has no scissor -- `View::paint` says so in as many
/// words ("no automatic clipping") -- so a scrolled panel has to cull
/// its own slots or it draws over the header above it and the terminal
/// below. A card is clipped to the viewport rather than dropped, so
/// the background continues to the edge and the panel reads as having
/// more below; a row, a separator and a line of text are dropped
/// unless they fit whole, because half a row and half a glyph are what
/// the clip would have prevented.
pub fn walk_visible(
    rect: Rect,
    scroll: f64,
    s: &Settings,
    m: Measure<'_>,
    mut on: impl FnMut(Slot),
) {
    let content = PanelChrome::new(rect, 0.0).content;
    let top = content.y_top;
    let bottom = content.y_top + content.h;
    let inside = |y_top: f64, h: f64| y_top >= top - 1e-9 && y_top + h <= bottom + 1e-9;
    walk(rect, scroll, s, m, |slot| match slot {
        Slot::Card { rect: c } => {
            let y = c.y_top.max(top);
            let h = (c.y_top + c.h).min(bottom) - y;
            if h > 0.0 {
                on(Slot::Card { rect: Rect { y_top: y, h, ..c } });
            }
        }
        Slot::Separator { y, .. } if !inside(y, card::SEPARATOR_PX) => {}
        Slot::Row { band, .. } if !inside(band.y_top, band.h) => {}
        Slot::Group { top: t, .. } if !inside(t, spec::px(content::HEADING.cap())) => {}
        Slot::Footer { baseline }
            if !inside(baseline - spec::px(metric::FOOTER.cap()), spec::px(metric::FOOTER.cap())) => {}
        other => on(other),
    });
}

/// How far the content can be pushed up before its last pixel is at
/// the bottom of the content area.  Zero when it already fits.
pub fn max_scroll(rect: Rect, s: &Settings, m: Measure<'_>) -> f64 {
    (content_h(s, m) - PanelChrome::new(rect, 0.0).content.h).max(0.0)
}

/// `(width, height)` of this row's control, in physical px.
pub fn control_size(row: Row, s: &Settings, m: Measure<'_>) -> (f64, f64) {
    // a picker's size does not depend on its value, and building the
    // value is an allocation this, which runs every frame, must not make
    if let Some(arrows) = row.picker_arrows() {
        return crate::ui::tree::preset::picker_size(metric::picker_value_w_pt(arrows));
    }
    match row.control(s) {
        Control::Toggle(_) => controls::toggle_size(),
        Control::Segmented { options, .. } => controls::segmented_size(options, m),
        Control::Picker { arrows, .. } => crate::ui::tree::preset::picker_size(metric::picker_value_w_pt(arrows)),
    }
}

/// Left edge for a row's text.
pub fn text_x(rect: Rect) -> f64 {
    PanelChrome::new(rect, 0.0).content.x + spec::px(card::ROW_PAD_X)
}

#[cfg(test)]
#[path = "settings_modal_tests.rs"]
mod tests;
