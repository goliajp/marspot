//! marspot UI `components` — composite widgets that pair geometry +
//! paint.  Each is the smallest unit a feature would reach for when
//! it needs more than a bare `View`: a table, a scroll viewport,
//! a modal frame layout, etc.  Built on top of `core::View` /
//! `system::macos::*` primitives.

pub mod cc_usage_layout;
pub mod cc_usage_modal;
pub mod cc_usage_paint;
pub mod cc_usage_reset_lines;
pub mod cc_usage_view;
pub mod chrome_target;
pub mod controls;
pub mod panel_chrome;
pub mod panel_palette;
pub mod panel_script;
pub mod surface_stack;
pub mod settings_choices;
pub mod settings_modal;
pub mod settings_view;
pub mod settings_rows;
pub mod helpers_view;
pub(crate) mod title_helpers;
pub mod shortcuts_panel;
pub mod shortcuts_view;
pub mod scroll_view;
pub mod scrollbar;
pub mod process_panel;
pub mod process_view;
pub mod search_overlay;
pub mod pane_notice;
pub mod panel;
pub mod text_input;
pub mod list_view;
pub mod button;
pub mod grid_seams;
pub mod grid;
pub mod grid_item;
pub mod sidebar;
pub mod sidebar_view;
pub mod title_strip_view;
#[cfg(test)]
mod title_strip_view_tests;
#[cfg(test)]
mod sidebar_view_tests;
#[cfg(test)]
mod chrome_hit_view_tests;
pub mod table;
pub mod layout_modal;
pub mod layout_view;
pub mod context_menu;
pub mod context_menu_view;
pub mod toolbar_view;
pub mod view_line;
#[cfg(test)]
mod view_line_tests;
#[cfg(test)]
mod toolbar_view_tests;
#[cfg(test)]
mod context_menu_view_tests;
#[cfg(test)]
mod context_menu_beside_tests;
#[cfg(test)]
pub(crate) mod density_kit;
#[cfg(test)]
mod density_tests;
#[cfg(test)]
mod hit_density_tests;
#[cfg(test)]
mod centring_tests;
#[cfg(test)]
pub(crate) mod panel_snapshot_tests;
#[cfg(test)]
mod chrome_snapshot_tests;
#[cfg(test)]
mod reset_snapshot_tests;
#[cfg(test)]
mod helper_strip_view_tests;
#[cfg(test)]
mod settings_view_tests;
#[cfg(test)]
mod settings_rows_view_tests;
#[cfg(test)]
mod settings_font_tests;
#[cfg(test)]
mod shortcuts_view_tests;
#[cfg(test)]
mod helpers_view_tests;
#[cfg(test)]
mod layout_view_tests;
#[cfg(test)]
mod process_view_tests;
#[cfg(test)]
mod process_left_snapshot_tests;
#[cfg(test)]
mod settings_agents_snapshot_tests;
#[cfg(test)]
mod panel_frames_view_tests;

#[cfg(test)]
mod language_snapshot_tests;

pub use scroll_view::ScrollView;
pub use panel_chrome::PanelChrome;
pub use search_overlay::{SearchOverlayParams, paint_search_overlay};
pub use panel::Panel;
pub use text_input::{TextInput, TextInputStyle};
pub use list_view::{ListView, ListRow, ListViewStyle};
pub use button::{Button, ButtonStyle, IconPosition};
pub use grid_seams::{GridSeams, SeamStyle};
pub use grid::{Grid, GridStyle};
pub use grid_item::{GridItem, GridEdges, Outline};
pub use sidebar::Sidebar;
pub use table::{Align, Column, ColumnWidth, SortDir, TableGeometry};
pub use layout_modal::{LayoutModal, GRID_MIN, GRID_MAX};
pub use context_menu::{ContextMenu, ContextMenuHit, MenuItem, dividers_sound, grouped};
