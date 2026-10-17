//! Theme — semantic colors + spacing + radius tokens.
//!
//! All components reference
//! these by name; literal `Color::rgba(...)` calls in component
//! code are an anti-pattern and will be cleaned up in P3i.

pub mod panel_type;
pub mod token;

pub use panel_type::PanelText;
pub use token::*;
