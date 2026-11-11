//! Handing a pane's conversation between claude, codex, gemini and kimi (RFC-009).

mod carried;
mod claude;
mod codex;
mod codex_stop;
mod gemini;
mod kimi;
pub(crate) mod history;
pub(crate) use marspot_peer::json;
mod ledger;
mod resume;
mod switch;
mod tags;
mod transcript;

pub use carried::remember_gemini_model;
pub(crate) use resume::resume_word;
pub use switch::{menu_rows, profiles, switch_op, Leaving};
pub use tags::{parse_tag, tag_for};
pub use transcript::Agent;
