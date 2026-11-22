//! The main window's own chrome: the toolbar in the header band, the
//! sidebar, a pane's title strip and seams, the search overlay, the
//! context menu.
//!
//! **Units.**  Points, as everywhere else in the spec: [`px`] is
//! [`super::px`].  The window's layout (`marspot_term::layout`) still
//! works in window units, half a point, one physical pixel at display
//! scale 1; the numbers it defines are halved where they are named
//! here.  The spacing ladder applies to these lengths doubled, the
//! window unit being where it was laid down.
//!
//! **The context menu** is chrome and is measured in points like the
//! rest; its labels take a panel type role.  Its geometry
//! (`ContextMenu::layout`) still steps in window units and doubles
//! these lengths to get them (`a_menu_row_is_measured_in_points_at_every_scale`).
//!
//! **One definition.**  `marspot_term::layout` places the toolbar
//! buttons and the sidebar's rows and buttons, and click handling asks
//! it where they are; that crate cannot depend on this one.  The
//! numbers it needs are therefore defined there and only named here.
//!
//! **Type.**  A pane's name, an agent's badge, the version, a search
//! query and its match count are identifiers and figures: they are set
//! in the terminal's mono cell, like the text they sit beside, and line
//! up with it.  Words addressed to the user — a menu item, a button's
//! label — take a [`PanelText`](super::PanelText) role.

use super::{panel, space};
use marspot_term::layout as window;

/// Physical pixels for `pt` points.
pub fn px(pt: f64) -> f64 {
    super::px(pt)
}

/// Side of a toolbar button.  Defined by the layout.
pub const TOOLBAR_BUTTON: f64 = window::ICON_BUTTON_LOGICAL_SIZE * 0.5;
/// Between two toolbar buttons.  Defined by the layout.
pub const TOOLBAR_GAP: f64 = window::ICON_BUTTON_LOGICAL_GAP * 0.5;
/// Corner radius of a toolbar button's hover and on fill.
pub const TOOLBAR_RADIUS: f64 = 2.0;
/// Point size of the symbol in a toolbar button, in points of the
/// spec; the symbol is drawn at a type size of twice this many window
/// units.  A symbol's frame is about 1.15 type sizes wide and 0.9
/// tall, the widest 1.3: at 6.5 the widest leaves the button's hover
/// fill a point clear on each side.
pub const TOOLBAR_ICON: f64 = 6.5;
/// The version label's right edge to the window's.
pub const VERSION_PAD_RIGHT: f64 = space::S6;

/// Height of a sidebar row.  Defined by the layout.
pub const SIDEBAR_ROW_H: f64 = window::SIDEBAR_ROW_H * 0.5;
/// The hover and selected fill of a row stops this far short of the
/// sidebar's edges.
pub const SIDEBAR_ROW_INSET: f64 = space::S2;
/// Corner radius of that fill and of the add button's.
pub const SIDEBAR_ROW_RADIUS: f64 = 2.0;
/// The status dot's left edge from the sidebar's.
pub const SIDEBAR_PAD_X: f64 = space::S6;
/// Diameter of the status dot.
pub const SIDEBAR_DOT: f64 = 4.0;
/// The dot to the pane's name.
pub const SIDEBAR_DOT_GAP: f64 = space::S4;
/// The name ends at least this far before the close button, whether
/// or not the button is showing: a name must not jump when the pointer
/// arrives.
pub const SIDEBAR_LABEL_GAP: f64 = space::S2;
/// Point size of the `+` in the add button.
pub const SIDEBAR_ADD_ICON: f64 = 6.5;

/// Height of a pane's title strip.  Its text starts at the terminal's
/// own inset (`Layout::padding`), above the first column.
pub const TITLE_H: f64 = crate::ui::CELL_TITLE_PT * 0.5;
/// Least distance between a pane's name and its badge.
pub const TITLE_GAP: f64 = space::S4;
/// A seam between two panes is this many layout gutters thick; the
/// focus ring replaces it at the same thickness.
pub const SEAM_GUTTERS: f64 = 4.0;
/// Opacity of the window's surface over a seat nothing runs in.
pub const EMPTY_SEAT_ALPHA: f32 = 0.6;
/// Opacity of the accent inside the outline of a drop target.
pub const DROP_TARGET_ALPHA: f32 = 0.25;
/// Corner radius and border of that outline.
pub const DROP_TARGET_RADIUS: f64 = 2.0;
pub const DROP_TARGET_BORDER: f64 = 1.0;

/// A popover — the search overlay, the context menu — is framed like a
/// panel at the window's density: half the panel's numbers.
pub const POPOVER_RADIUS: f64 = panel::RADIUS * 0.5;
pub const POPOVER_SHADOW_BLUR: f64 = panel::SHADOW_BLUR * 0.5;
/// Corner radius of the fill behind a popover's picked or hovered row.
pub const POPOVER_ROW_RADIUS: f64 = 2.0;
/// That fill stops this far short of the popover's left and right edges.
pub const POPOVER_ROW_INSET: f64 = space::S2;

/// Least inset of the search overlay's content from its frame; the
/// inset is a fraction of the terminal's line above that.
pub const SEARCH_PAD_MIN: f64 = space::S6 * 0.5;
/// Width of the caret in the search field, and how far it stops short
/// of the field's top and bottom.
pub const SEARCH_CARET_W: f64 = 1.0;
pub const SEARCH_CARET_INSET: f64 = 1.0;

/// Height of a context menu row.
pub const MENU_ROW_H: f64 = 13.0;
/// Height of the row a divider takes.
pub const MENU_DIVIDER_H: f64 = space::S4;
/// A menu row's text from the frame's left and right edges.
pub const MENU_PAD_X: f64 = space::S6;
/// The first and last rows from the frame's top and bottom edges.
pub const MENU_PAD_Y: f64 = space::S6 * 0.5;
/// Narrowest and widest a menu is, whatever its labels.
pub const MENU_MIN_W: f64 = 90.0;
pub const MENU_MAX_W: f64 = 180.0;

/// Width of the thumb of a pane's scrollbar, shown while the pane is
/// scrolled back into its history.
pub const SCROLLBAR_W: f64 = 3.0;
/// The thumb's distance from the pane's right edge and from the ends
/// of its track.
pub const SCROLLBAR_INSET: f64 = space::S2;
/// Shortest the thumb gets, however deep the history.
pub const SCROLLBAR_MIN_THUMB: f64 = space::S16;
/// How far in from the pane's right edge a press lands on the
/// scrollbar: wider than the thumb, which is too thin to aim at.
pub const SCROLLBAR_HIT_W: f64 = space::S8;
