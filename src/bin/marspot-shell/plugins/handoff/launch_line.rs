//! The command line that starts an agent under one of its account
//! homes: for a pane moving to another agent, and for a helper.

use std::path::Path;

use super::history;
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

/// The model a pane moving to `to` under `home` starts on.
///
/// claude and codex read the model they start on from a settings file
/// every account shares, which a model picked in any session rewrites;
/// so claude always names one (the resumed session's, else the
/// account's last observed model, else the default) and codex names the
/// one its rollouts last recorded. gemini carries what the session it
/// resumes was set to.
pub(crate) fn model_for(to: Agent, home: &Path, resume: Resume, gemini_model: Option<&str>) -> Option<String> {
    use crate::plugins::claudecode::launch_model::{self, Source};
    use crate::plugins::codex::last_model;
    match (to, resume) {
        (Agent::Claude, _) => {
            let session = || match resume {
                Resume::Id(id) => launch_model::session_model(id, history::claude_transcript(home, id).as_deref()),
                Resume::New | Resume::Last => None,
            };
            let (model, from) = launch_model::first_known(&[
                (Source::Session, &session),
                (Source::Account, &|| launch_model::account_model(home)),
            ]);
            marspot::lx_info!("handoff.model_from", &format!("{} {model} ({})", home.display(), from.as_str()));
            Some(model)
        }
        (Agent::Codex, Resume::Id(id)) => last_model::thread_model(home, id),
        (Agent::Codex, _) => last_model::account_model(home),
        (Agent::Gemini, _) => gemini_model.map(str::to_string),
        (Agent::Kimi, _) => None,
    }
}

/// `to` under `home`; None when a part of the line cannot be quoted, or
/// for claude when no model is given: a claude started without one runs
/// on the shared settings file's default.
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
            word(c.arg("--model"), l.model?)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane moving to claude starts on the resumed session's model,
    /// else its account's last observed one, else the default; to codex,
    /// on the model the resumed rollout or the account last named.
    #[test]
    fn handoff_launch_names_the_model() {
        let state = marspot_term::test_env::sandbox_state_dir();
        let home = marspot_term::test_env::TempDir::new("handoff-model");
        let claude = |r| model_for(Agent::Claude, home.path(), r, None);
        assert_eq!(claude(Resume::New).as_deref(), Some(crate::plugins::claudecode::launch_model::DEFAULT_MODEL));
        let push = state.path().join("plugins/claudecode/model");
        std::fs::create_dir_all(&push).unwrap();
        let t = home.path().join("projects/-w/u-2.jsonl");
        std::fs::write(push.join("u-2"), format!("x\n{}\nhigh\npids=1\nid=claude-opus-5-5\n", t.display())).unwrap();
        assert_eq!(claude(Resume::New).as_deref(), Some("claude-opus-5-5"), "the account's");
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        std::fs::write(home.path().join("projects/-w/u-1.jsonl"), r#"{"message":{"role":"assistant","model":"claude-fable-5-1"}}"#).unwrap();
        assert_eq!(claude(Resume::Id("u-1")).as_deref(), Some("claude-fable-5-1"), "the resumed session's");
        let l = Launch { resume: Resume::Id("u-1"), model: None, flags: &[] };
        assert!(agent_command(Agent::Claude, home.path(), &l).is_none(), "no claude line without a model");

        let codex = |r| model_for(Agent::Codex, home.path(), r, None);
        assert_eq!(codex(Resume::New), None, "nothing observed");
        let day = home.path().join("sessions/2026/11/27");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-11-27T00-00-00-t-1.jsonl"), "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.5\"}}\n").unwrap();
        assert_eq!(codex(Resume::Id("t-1")).as_deref(), Some("gpt-5.5"));
        assert_eq!(codex(Resume::New).as_deref(), Some("gpt-5.5"));
        let line = String::from_utf8(agent_command(Agent::Codex, home.path(), &Launch { resume: Resume::New, model: Some("gpt-5.5"), flags: &[] }).unwrap().to_bytes().unwrap()).unwrap();
        assert!(line.contains("codex -m gpt-5.5"), "{line}");
        assert_eq!(model_for(Agent::Gemini, home.path(), Resume::New, Some("flash")).as_deref(), Some("flash"));
        assert_eq!(model_for(Agent::Kimi, home.path(), Resume::New, None), None);
    }
}
