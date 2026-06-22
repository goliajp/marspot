//! Theme — semantic colors + spacing + radius tokens.
//!
//! See `docs/ui-system-model.md` §2.3.  All components reference
//! these by name; literal `Color::rgba(...)` calls in component
//! code are an anti-pattern and will be cleaned up in P3i.

pub mod token;

pub use token::*;
