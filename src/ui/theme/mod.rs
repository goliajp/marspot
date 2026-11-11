//! Theme — semantic colors + spacing + radius tokens.
//!
//! Components refer to these by name and never write a colour out;
//! `components::panel_chrome` has the test that holds them to it.

pub mod contrast;
pub mod panel_type;
pub mod spec;
pub mod table;
pub mod terminal;
pub mod type_scale;

pub use panel_type::PanelText;
pub use table::{color, current, Theme};
