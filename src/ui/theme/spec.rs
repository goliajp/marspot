//! The measurements every panel is built from.
//!
//! A panel is a frame, a header and a content area, top to bottom, and
//! its content is groups: a heading and a card.  Every distance and
//! size in that structure has a name here, and panel code refers to
//! the name.
//!
//! Rules for panel authors:
//!
//! - **No colour literals.**  Colours come from
//!   `ui::components::panel_palette`, which maps theme tokens to roles.
//! - **No bare sizes.**  Text is set in a [`PanelText`] role; every
//!   length is a constant from this module.
//! - **Spacing only from the ladder** ([`space::LADDER`]).  The gap
//!   between any two elements is one of its steps; the named gaps in
//!   [`space`] cover the usual positions.
//! - **One primary button** per panel at most, at the right of the
//!   footer, never stretched across the row.
//! - **Sentence-case titles**: `Process monitor`, not `Process Monitor`
//!   and not `PROCESS MONITOR`.
//! - **Selected and on are the accent**, everywhere.  Green, yellow and
//!   red say how something is doing, never that it is picked.
//!
//! Everything is in logical points; [`px`] takes a value to physical
//! pixels.  The exceptions are [`table`], which is measured in
//! terminal cells because tabular data is set in the terminal's own
//! font, where columns line up for free, and [`chrome`], the main
//! window's own chrome, which says what its units are.

use super::PanelText;

pub mod chrome;

/// Physical pixels for `pt` logical points.
pub fn px(pt: f64) -> f64 {
    pt * crate::ui::core::ViewPainter::px_per_pt()
}

/// The spacing ladder, and the gaps that are always the same step.
pub mod space {
    pub const S2: f64 = 2.0;
    pub const S4: f64 = 4.0;
    pub const S6: f64 = 6.0;
    pub const S8: f64 = 8.0;
    pub const S12: f64 = 12.0;
    pub const S16: f64 = 16.0;
    pub const S20: f64 = 20.0;
    pub const S24: f64 = 24.0;
    pub const S32: f64 = 32.0;

    /// Every distance between two elements is one of these.
    pub const LADDER: [f64; 9] = [S2, S4, S6, S8, S12, S16, S20, S24, S32];

    /// A group heading's baseline to the top of its card.
    pub const HEADING_TO_CARD: f64 = S8;
    /// A card's bottom to the top of the next group heading's line.
    pub const CARD_TO_HEADING: f64 = S20;
    /// Between two cards of one group, side by side or stacked.
    pub const CARD_TO_CARD: f64 = S12;
    /// A card's bottom to the footer (a caption or a button row).
    pub const CARD_TO_FOOTER: f64 = S16;
    /// Between two controls on one row.
    pub const CONTROL_TO_CONTROL: f64 = S6;
    /// A label to the explanatory line under it.
    pub const LABEL_TO_DESC: f64 = S4;
}

/// The frame every panel is drawn in.
pub mod panel {
    /// Corner radius of the frame.
    pub const RADIUS: f64 = 10.0;
    /// Border width, in physical pixels: one device pixel at any scale.
    pub const BORDER_PX: f32 = 1.0;
    /// Blur radius of the frame's shadow.
    pub const SHADOW_BLUR: f64 = 16.0;
    /// Opacity of the frame's shadow.
    pub const SHADOW_ALPHA: f32 = 0.45;
    /// How dark the rest of the window goes while a panel is open.
    pub const BACKDROP_ALPHA: f32 = 0.45;
    /// Widest a panel may be, as a fraction of the window's width.
    pub const MAX_W_FRAC: f64 = 0.96;
    /// Tallest a panel may be, as a fraction of the window's height.
    pub const MAX_H_FRAC: f64 = 0.92;
    /// Least distance from the window's top chrome to the frame.
    pub const TOP_CLEARANCE: f64 = super::space::S8;
}

/// The header: title at the left, close button at the right.
pub mod header {
    use super::PanelText;

    /// Header height.
    pub const H: f64 = 40.0;
    /// The title's type role.
    pub const TITLE: PanelText = PanelText::Title;
    /// The optional line left of the close button ("updated 11/10").
    pub const META: PanelText = PanelText::Caption;
    /// Side of the close button's square hit box.
    pub const CLOSE_SIZE: f64 = 22.0;
    /// The close button's right edge to the frame's right edge.
    pub const CLOSE_PAD_RIGHT: f64 = super::space::S12;
    /// Width and height of the × drawn in the close button.
    pub const CLOSE_MARK: f64 = 9.0;
    /// Corner radius of the close button's hover fill.
    pub const CLOSE_RADIUS: f64 = 5.0;
    /// The meta line's right edge to the close button's left edge.
    pub const META_GAP: f64 = super::space::S12;
}

/// The content area under the header.
pub mod content {
    /// Inset from the frame's left and right edges; the title starts
    /// at the same x.
    pub const PAD_X: f64 = 18.0;
    /// Inset from the frame's bottom edge.
    pub const PAD_BOTTOM: f64 = 18.0;
    /// The group heading's type role.
    pub const HEADING: super::PanelText = super::PanelText::Section;
    /// The type role of the line at a heading's right.
    pub const HEADING_TRAILING: super::PanelText = super::PanelText::Caption;
}

/// A card: the surface a group's rows or table sit on.
pub mod card {
    /// Corner radius.
    pub const RADIUS: f64 = 8.0;
    /// Border width, in physical pixels.
    pub const BORDER_PX: f32 = 1.0;
    /// A row's inset from the card's left and right edges.
    pub const ROW_PAD_X: f64 = super::space::S12;
    /// A settings-style row's inset above its first line and below its
    /// last.
    pub const ROW_PAD_Y: f64 = super::space::S8;
    /// Thickness of the line between two rows, in physical pixels.
    pub const SEPARATOR_PX: f64 = 1.0;
    /// The separator's inset from the card's left and right edges.
    pub const SEPARATOR_INSET: f64 = super::space::S12;
}

/// Tabular data.  Measured in terminal cells, not points.
pub mod table {
    /// Header row height, in cell heights.
    pub const HEADER_H_CELLS: f64 = 1.5;
    /// Data row height, in cell heights.
    pub const ROW_H_CELLS: f64 = 1.35;
    /// Least gap between two columns, in cell widths.
    pub const COLUMN_GAP_CELLS: f64 = 2.0;
    /// Indent per tree level, in cell widths.
    pub const INDENT_CELLS: f64 = 2.0;
    /// Opacity of the accent behind the selected row.
    pub const SELECTED_ALPHA: f32 = 0.35;
    /// Corner radius of the selected row's fill, in points.
    pub const SELECTED_RADIUS: f64 = 4.0;
    /// The selected row's fill stops this far short of the card's left
    /// and right edges, in points.
    pub const SELECTED_INSET: f64 = super::space::S4;

    /// Header row height in physical pixels for a cell `cell_h` tall.
    pub fn header_h(cell_h: f64) -> f64 {
        cell_h * HEADER_H_CELLS
    }

    /// Data row height in physical pixels for a cell `cell_h` tall.
    pub fn row_h(cell_h: f64) -> f64 {
        cell_h * ROW_H_CELLS
    }
}

/// Buttons, segmented controls, toggles, steppers, chips.
pub mod control {
    use super::PanelText;

    /// How far a hovered fill moves towards white.
    pub const HOVER_LIGHTEN: f32 = 0.08;
    /// Opacity of a disabled toggle.
    pub const DISABLED_ALPHA: f32 = 0.5;
    /// Stroke thickness of the × in a close or danger button.
    pub const CROSS_STROKE: f64 = 1.5;

    /// Height of a primary or secondary button.
    pub const BUTTON_H: f64 = 28.0;
    /// Least width of a primary or secondary button.
    pub const BUTTON_MIN_W: f64 = 88.0;
    /// Corner radius of a button.
    pub const BUTTON_RADIUS: f64 = 6.0;
    /// A button's inset left and right of its label.
    pub const BUTTON_PAD_X: f64 = 16.0;
    /// A button label's type role.
    pub const BUTTON_TEXT: PanelText = PanelText::Label;

    /// Height of one segment of a segmented control.
    pub const SEGMENT_H: f64 = 22.0;
    /// Corner radius of a segment.
    pub const SEGMENT_RADIUS: f64 = 5.0;
    /// A segment's inset left and right of its label.
    pub const SEGMENT_PAD_X: f64 = 10.0;
    /// Between two segments.
    pub const SEGMENT_GAP: f64 = super::space::CONTROL_TO_CONTROL;
    /// A segment label's type role.
    pub const SEGMENT_TEXT: PanelText = PanelText::Item;

    /// Toggle track width.
    pub const TOGGLE_W: f64 = 34.0;
    /// Toggle track height; the track's ends are full half-circles.
    pub const TOGGLE_H: f64 = 20.0;
    /// Diameter of the toggle's knob.
    pub const TOGGLE_KNOB: f64 = 16.0;

    /// Side of each of a stepper's two square buttons.
    pub const STEPPER_BUTTON: f64 = 22.0;
    /// Width of the value between a stepper's buttons.
    pub const STEPPER_VALUE_W: f64 = 32.0;

    /// Side of the danger icon button (end a process).
    pub const DANGER_SIZE: f64 = 18.0;
    /// Corner radius of the danger icon button.
    pub const DANGER_RADIUS: f64 = 4.0;
    /// Opacity of the danger colour behind the × at rest.
    pub const DANGER_REST_ALPHA: f32 = 0.16;
    /// Width and height of the × in the danger icon button.
    pub const DANGER_MARK: f64 = 8.0;

    /// Height of a status chip (`OK`, `LIMIT`).
    pub const CHIP_H: f64 = 16.0;
    /// Corner radius of a status chip.
    pub const CHIP_RADIUS: f64 = 4.0;
    /// A chip's inset left and right of its label.
    pub const CHIP_PAD_X: f64 = 6.0;
    /// Opacity of the status colour behind a chip's label.
    pub const CHIP_FILL_ALPHA: f32 = 0.16;
    /// A chip label's type role.
    pub const CHIP_TEXT: PanelText = PanelText::Caption;
    /// A chip label's weight: the caption role, set bold.
    pub const CHIP_WEIGHT: u16 = 700;

    /// A key cap's inset left and right of its glyphs.
    pub const KEYCAP_PAD_X: f64 = 6.0;
    /// A key cap's inset above and below its line of glyphs.
    pub const KEYCAP_PAD_Y: f64 = 2.0;
    /// Corner radius of a key cap.
    pub const KEYCAP_RADIUS: f64 = 4.0;
    /// Between two key caps on one row.
    pub const KEYCAP_GAP: f64 = super::space::CONTROL_TO_CONTROL;
    /// A key cap's glyphs: the size of a menu item, a step heavier so
    /// the thin modifier signs hold up on the cap's fill.
    pub const KEYCAP_TEXT: PanelText = PanelText::Item;
    pub const KEYCAP_WEIGHT: u16 = 500;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gap that is not a step of the ladder is a new spacing value
    /// nobody agreed to, however close it is to one that exists.
    #[test]
    fn spec_every_gap_is_a_step_of_the_ladder() {
        let gaps = [
            ("space::HEADING_TO_CARD", space::HEADING_TO_CARD),
            ("space::CARD_TO_HEADING", space::CARD_TO_HEADING),
            ("space::CARD_TO_CARD", space::CARD_TO_CARD),
            ("space::CARD_TO_FOOTER", space::CARD_TO_FOOTER),
            ("space::CONTROL_TO_CONTROL", space::CONTROL_TO_CONTROL),
            ("space::LABEL_TO_DESC", space::LABEL_TO_DESC),
            ("panel::TOP_CLEARANCE", panel::TOP_CLEARANCE),
            ("header::CLOSE_PAD_RIGHT", header::CLOSE_PAD_RIGHT),
            ("header::META_GAP", header::META_GAP),
            ("card::ROW_PAD_X", card::ROW_PAD_X),
            ("card::ROW_PAD_Y", card::ROW_PAD_Y),
            ("card::SEPARATOR_INSET", card::SEPARATOR_INSET),
            ("table::SELECTED_INSET", table::SELECTED_INSET),
            ("control::SEGMENT_GAP", control::SEGMENT_GAP),
            ("chrome::TOOLBAR_GAP", chrome::TOOLBAR_GAP),
            ("chrome::VERSION_PAD_RIGHT", chrome::VERSION_PAD_RIGHT),
            ("chrome::SIDEBAR_ROW_INSET", chrome::SIDEBAR_ROW_INSET),
            ("chrome::SIDEBAR_PAD_X", chrome::SIDEBAR_PAD_X),
            ("chrome::SIDEBAR_DOT_GAP", chrome::SIDEBAR_DOT_GAP),
            ("chrome::SIDEBAR_LABEL_GAP", chrome::SIDEBAR_LABEL_GAP),
            ("chrome::TITLE_GAP", chrome::TITLE_GAP),
            ("chrome::POPOVER_ROW_INSET", chrome::POPOVER_ROW_INSET),
            ("chrome::MENU_DIVIDER_H", chrome::MENU_DIVIDER_H),
            ("chrome::MENU_PAD_X", chrome::MENU_PAD_X),
            ("chrome::MENU_PAD_Y", chrome::MENU_PAD_Y),
            ("control::KEYCAP_GAP", control::KEYCAP_GAP),
        ];
        for (name, v) in gaps {
            assert!(space::LADDER.contains(&v), "{name} = {v} is not on the ladder");
        }
        // The check has to be able to fail: a value between two steps
        // is not on the ladder.
        assert!(!space::LADDER.contains(&7.0));
        assert!(space::LADDER.windows(2).all(|w| w[0] < w[1]), "the ladder ascends");
    }

    /// The numbers the four panels have to agree on, pinned: a change
    /// here is a change to every panel at once and should read as one.
    #[test]
    fn spec_pins_the_shared_structure() {
        assert_eq!(header::H, 40.0);
        assert_eq!(content::PAD_X, 18.0);
        assert_eq!(content::PAD_BOTTOM, 18.0);
        assert_eq!(panel::RADIUS, 10.0);
        assert_eq!(card::RADIUS, 8.0);
        assert_eq!((header::CLOSE_SIZE, header::CLOSE_PAD_RIGHT), (22.0, 12.0));
        assert_eq!((control::TOGGLE_W, control::TOGGLE_H, control::TOGGLE_KNOB), (34.0, 20.0, 16.0));
        assert_eq!((control::BUTTON_H, control::BUTTON_MIN_W), (28.0, 88.0));
        // The close button and the knob have to fit what holds them.
        const { assert!(header::CLOSE_SIZE < header::H) };
        const { assert!(control::TOGGLE_KNOB < control::TOGGLE_H) };
        // The window's chrome: what the layout crate defines is what
        // the spec names, and a control fits what holds it.
        assert_eq!((chrome::TOOLBAR_BUTTON, chrome::TOOLBAR_GAP), (22.0, 6.0));
        assert_eq!((chrome::SIDEBAR_ROW_H, chrome::TITLE_H), (22.0, 22.0));
        assert_eq!((chrome::MENU_ROW_H, chrome::MENU_PAD_X, chrome::MENU_PAD_Y), (26.0, 12.0, 6.0));
        const { assert!(chrome::TOOLBAR_ICON < chrome::TOOLBAR_BUTTON) };
        const { assert!(chrome::SIDEBAR_DOT < chrome::SIDEBAR_ROW_H) };
        // A row's fill is concentric with the menu's own corner.
        const { assert!(chrome::POPOVER_ROW_RADIUS + chrome::MENU_PAD_Y == chrome::POPOVER_RADIUS) };
        assert_eq!(table::row_h(20.0), 27.0);
        assert_eq!(table::header_h(20.0), 30.0);
    }
}
