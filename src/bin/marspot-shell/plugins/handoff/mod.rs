//! Handing a pane's conversation between claude, codex, gemini and kimi (RFC-009).

mod carried;
mod claude;
mod codex;
mod codex_stop;
mod gemini;
mod kimi;
pub(crate) mod launch_line;
pub(crate) mod history;
pub(crate) use marspot_peer::json;
mod ledger;
mod menu;
mod note;
mod resume;
mod switch;
mod tags;
mod transcript;

pub use carried::remember_gemini_model;
pub(crate) use resume::resume_word;
pub use menu::{menu_rows, profiles};
pub use switch::{switch_op, Leaving};
pub(crate) use ledger::write_atomic;
pub use tags::{parse_tag, profile_tag, Pick};
pub use transcript::Agent;
