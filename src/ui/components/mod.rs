//! marspot UI `components` — composite widgets that pair geometry +
//! paint.  Each is the smallest unit a feature would reach for when
//! it needs more than a bare `View`: a tab strip, a scroll viewport,
//! a modal frame layout, etc.  Built on top of `core::View` /
//! `system::macos::*` primitives.

pub mod cc_usage_modal;
pub mod tab_strip;
pub mod scroll_view;
pub mod modal_frame;
pub mod search_overlay;
pub mod panel;
pub mod text_input;
pub mod list_view;
pub mod button;
pub mod grid_seams;
pub mod grid;
pub mod grid_item;
pub mod sidebar;
pub mod table;
pub mod layout_modal;
pub mod context_menu;
pub mod dev_panel;

pub use tab_strip::TabStrip;
pub use scroll_view::ScrollView;
pub use modal_frame::ModalFrame;
pub use search_overlay::{SearchOverlayParams, paint_search_overlay};
pub use panel::Panel;
pub use text_input::{TextInput, TextInputStyle};
pub use list_view::{ListView, ListRow, ListViewStyle};
pub use button::{Button, ButtonStyle, IconSpec, IconPosition};
pub use grid_seams::{GridSeams, SeamStyle};
pub use grid::{Grid, GridStyle};
pub use grid_item::{GridItem, GridEdges, Outline};
pub use sidebar::{Sidebar, SidebarRow, SidebarStyle};
pub use table::{Table, TableColumn, TableRow, TableStyle, ColumnWidth, RowKind, SortDir};
pub use layout_modal::{LayoutModal, LayoutModalHit, GRID_MIN, GRID_MAX};
pub use context_menu::{ContextMenu, ContextMenuHit, MenuItem};
pub use dev_panel::{DevPanelState, DevPanelHit, build_dev_panel_canvas, hit_test, DEV_PANEL_MODEL_SCROLL_ID, scroll_id_for_section};
