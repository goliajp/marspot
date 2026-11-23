//! The command line that starts an agent under one of its account
//! homes: for a pane moving to another agent, and for a helper.

use std::path::Path;

use super::transcript::Agent;
use crate::plugins::pty_op::PtyCommand;
use crate::plugins::{gemini, kimi};

/// Which conversation the agent opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resume<'a> {
    New,
    Id(&'a str),
    /// The newest one in the directory the line runs in.
    Last,
}

pub(crate) struct Launch<'a> {
    pub resume: Resume<'a>,
    pub model: Option<&'a str>,
    /// Flags added as they are (a permission mode, say).
    pub flags: &'a [String],
}

/// `to` under `home`; None when a part of the line cannot be quoted.
pub(crate) fn agent_command(to: Agent, home: &Path, l: &Launch) -> Option<PtyCommand> {
    let h = home.to_str()?;
    let model = |c: PtyCommand, flag: &str| match l.model {
        Some(m) => word(c.arg(flag), m),
        None => c,
    };
    let cmd = match to {
        Agent::Claude => {
            // the default home takes no variable: claude names its
            // keychain entry differently once the variable is set
            let c = PtyCommand::new("claude");
            let c = if home == crate::plugins::claudecode::config_dir_or_default(None) { c } else { c.env("CLAUDE_CONFIG_DIR", h) };
            let c = crate::plugins::peerbus::claude_mcp::with_config(c);
            // No submit receipt hook -- see `receipts::claude_settings_arg`
            // for why nothing asks for one any more.
            let c = match l.resume {
                Resume::Id(id) => c.arg("--resume").arg(id),
                Resume::Last => c.arg("--continue"),
                Resume::New => c,
            };
            model(c, "--model")
        }
        Agent::Codex => {
            let c = PtyCommand::new("codex").env("CODEX_HOME", h);
            let c = match l.resume {
                Resume::Id(id) => c.arg("resume").arg(id),
                Resume::Last => c.arg("resume").arg("--last"),
                Resume::New => c,
            };
            model(c, "-m")
        }
        Agent::Gemini => {
            let c = model(gemini::launch(home)?, "-m");
            match l.resume {
                Resume::Id(id) => c.arg("--resume").arg(id),
                Resume::Last => c.arg("--resume").arg("latest"),
                Resume::New => c,
            }
        }
        Agent::Kimi => match l.resume {
            Resume::Id(id) => kimi::launch(home, Some(id))?,
            Resume::Last => kimi::launch(home, None)?.arg("-c"),
            Resume::New => model(kimi::launch(home, None)?, "-m"),
        },
    };
    Some(l.flags.iter().fold(cmd, |c, f| word(c, f)))
}

/// `v` as one word: bare when it is a plain token, single-quoted
/// otherwise, so a space or a glob in a caller's text cannot split or
/// expand it.
fn word(c: PtyCommand, v: &str) -> PtyCommand {
    let plain = !v.is_empty() && v.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.:/@+=,".contains(ch));
    if plain { c.arg(v) } else { c.quoted_arg(v) }
}
