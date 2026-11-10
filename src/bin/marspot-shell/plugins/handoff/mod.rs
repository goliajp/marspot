//! Handing a pane's conversation between claude, codex and gemini (RFC-009).

mod claude;
mod codex;
mod codex_stop;
mod gemini;
pub(crate) mod history;
pub(crate) use marspot_peer::json;
mod ledger;
mod resume;
mod switch;
mod transcript;

pub(crate) use resume::resume_word;
pub use switch::{menu_rows, parse_tag, profiles, switch_op, Leaving};
pub use transcript::Agent;
