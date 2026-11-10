//! Theme — semantic colors + spacing + radius tokens.
//!
//! Components refer to these by name and never write a colour out;
//! `components::panel_chrome` has the test that holds them to it.

pub mod panel_type;
pub mod spec;
pub mod terminal;
pub mod token;

pub use panel_type::PanelText;
pub use token::*;
