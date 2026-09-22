//! Handing a pane's conversation between claude and codex (RFC-009).

mod claude;
mod codex;
mod history;
mod json;
mod ledger;
mod switch;
mod transcript;

pub use switch::{menu_rows, parse_tag, profiles, switch_op, Leaving};
pub use transcript::Agent;
