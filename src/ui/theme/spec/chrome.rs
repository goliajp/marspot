//! The main window's own chrome: the toolbar in the header band, the
//! sidebar, a pane's title strip and seams, the search overlay, the
//! context menu.
//!
//! **Units.**  The window is laid out at the terminal's density, not a
//! panel's: these numbers are *window units*, one physical pixel at
//! display scale 1 — half a panel point.  [`px`] takes one to physical
//! pixels.  The spacing ladder applies to them as it does to points.
//! The sidebar is the exception inside the exception: the layout gives
//! its rows a fixed height in physical pixels, so everything drawn in a
//! row is in physical pixels too.
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

/// Physical pixels for `units` window units.
pub fn px(units: f64) -> f64 {
    units * crate::ui::chrome_scale()
}

/// Side of a toolbar button.  Defined by the layout.
pub const TOOLBAR_BUTTON: f64 = window::ICON_BUTTON_LOGICAL_SIZE;
/// Between two toolbar buttons.  Defined by the layout.
pub const TOOLBAR_GAP: f64 = window::ICON_BUTTON_LOGICAL_GAP;
/// Corner radius of a toolbar button's hover and on fill.
pub const TOOLBAR_RADIUS: f64 = 4.0;
/// Side of the icon in a toolbar button.
pub const TOOLBAR_ICON: f64 = 14.0;
/// The version label's right edge to the window's.
pub const VERSION_PAD_RIGHT: f64 = space::S12;

/// Height of a sidebar row, physical px.  Defined by the layout.
pub const SIDEBAR_ROW_H: f64 = window::SIDEBAR_ROW_H_PHYS;
/// The hover and selected fill of a row stops this far short of the
/// sidebar's edges, physical px.
pub const SIDEBAR_ROW_INSET: f64 = space::S4;
/// Corner radius of that fill and of the add button's, physical px.
pub const SIDEBAR_ROW_RADIUS: f64 = 4.0;
/// The status dot's left edge from the sidebar's, physical px.
pub const SIDEBAR_PAD_X: f64 = space::S12;
/// Diameter of the status dot, physical px.
pub const SIDEBAR_DOT: f64 = 8.0;
/// The dot to the pane's name, physical px.
pub const SIDEBAR_DOT_GAP: f64 = space::S8;
/// The name ends at least this far before the close button, physical
/// px, whether or not the button is showing: a name must not jump when
/// the pointer arrives.
pub const SIDEBAR_LABEL_GAP: f64 = space::S4;
/// Width and height of the `+` in the add button, physical px.
pub const SIDEBAR_ADD_MARK: f64 = 9.0;
/// Stroke of that `+`, physical px.
pub const SIDEBAR_ADD_STROKE: f64 = 1.0;

/// Height of a pane's title strip.  Its text starts at the terminal's
/// own inset (`Layout::padding`), above the first column.
pub const TITLE_H: f64 = crate::ui::CELL_TITLE_PT;
/// Least distance between a pane's name and its badge.
pub const TITLE_GAP: f64 = space::S8;
/// A seam between two panes is this many layout gutters thick; the
/// focus ring replaces it at the same thickness.
pub const SEAM_GUTTERS: f64 = 4.0;
/// Opacity of the window's surface over a seat nothing runs in.
pub const EMPTY_SEAT_ALPHA: f32 = 0.6;
/// Opacity of the accent inside the outline of a drop target.
pub const DROP_TARGET_ALPHA: f32 = 0.25;
/// Corner radius and border of that outline.
pub const DROP_TARGET_RADIUS: f64 = 4.0;
pub const DROP_TARGET_BORDER: f64 = 2.0;

/// A popover — the search overlay, the context menu — is framed like a
/// panel at the window's density: the panel's numbers, in window units.
pub const POPOVER_RADIUS: f64 = panel::RADIUS;
pub const POPOVER_SHADOW_BLUR: f64 = panel::SHADOW_BLUR;
/// Corner radius of the fill behind a popover's picked or hovered row.
pub const POPOVER_ROW_RADIUS: f64 = 4.0;
/// That fill stops this far short of the popover's left and right edges.
pub const POPOVER_ROW_INSET: f64 = space::S4;

/// Height of a context menu row.
pub const MENU_ROW_H: f64 = 26.0;
/// Height of the row a divider takes.
pub const MENU_DIVIDER_H: f64 = space::S8;
/// A menu row's text from the frame's left and right edges.
pub const MENU_PAD_X: f64 = space::S12;
/// The first and last rows from the frame's top and bottom edges.
pub const MENU_PAD_Y: f64 = space::S6;
/// Narrowest and widest a menu is, whatever its labels.
pub const MENU_MIN_W: f64 = 180.0;
pub const MENU_MAX_W: f64 = 360.0;
