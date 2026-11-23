//! What asked for a helper: an agent, or a person or script at a shell.
//! Told from the processes the request came through, which are there
//! from its first moment; what the agent plugins say of the pane takes
//! them a scan or two to learn.

use std::sync::Arc;

use marspot::pidtree::{self, ProcRow};

use crate::observe::Proc;
use crate::pane_control::helpers_port::Port;

/// The agent `port`'s caller is, or `shell`.
pub(super) fn of(port: &Port) -> &'static str {
    match (port.caller_processes(), port.caller()) {
        (Some(chain), _) => among(&chain).unwrap_or("shell"),
        // named by its thread
        (None, Some(_)) => "codex",
        (None, None) => "shell",
    }
}

/// The agent nearest the start of `chain`, which runs from the process
/// that asked up to its pane's shell.
pub(super) fn among(chain: &[ProcRow]) -> Option<&'static str> {
    chain.iter().find_map(|row| {
        let p = Proc { row: row.clone(), argv: pidtree::proc_argv(row.pid).map(Arc::from) };
        match p.name() {
            "claude" => Some("claude"),
            "codex" => Some("codex"),
            _ if crate::plugins::gemini::is_gemini(&p) => Some("gemini"),
            _ if crate::plugins::kimi::looks_like_kimi(row) => Some("kimi"),
            _ => None,
        }
    })
}
