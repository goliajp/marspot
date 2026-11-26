//! The model a helper starts on, filled into its call before anything
//! reads it, so the account pick, the script and the record all see the
//! model the helper will run.
//!
//! A claude or codex started without a model takes the one in the
//! shared settings file, which any session can rewrite for every
//! account. A Claude helper always names one: what was asked, else the
//! conversation it goes on with, the parent pane's model, the
//! account's last observed model, the default. A codex helper names
//! what was asked, else what its rollouts last recorded.

use std::path::Path;

use marspot_peer::control::{AgentKind, HelperStartArgs};

use crate::plugins::claudecode::launch_model::{self, Source};

/// Before the account is chosen: a new Claude helper asked for no model
/// runs on its parent pane's, so the pick sees the model it will run.
/// `plain`: the allocation chose a plain model over Fable for this call.
pub(super) fn early(a: &HelperStartArgs, parent: u64, plain: bool) -> Option<HelperStartArgs> {
    let ours = a.agent == AgentKind::Claude && a.model.is_none() && !a.resume_last;
    let model = ours.then(|| inherited(launch_model::pane_model(parent), plain)).flatten()?;
    marspot::lx_info!("helpers.model_from", &format!("parent={parent} {model} (pane)"));
    Some(HelperStartArgs { model: Some(model), ..a.clone() })
}

/// Once the account under `home` and whether it goes on with the last
/// conversation in `cwd` are known: the model for a call that still
/// names none.
pub(super) fn late(a: &HelperStartArgs, parent: u64, home: &Path, cwd: &str, resume: bool, plain: bool) -> Option<HelperStartArgs> {
    if a.model.is_some() {
        return None;
    }
    let model = match a.agent {
        AgentKind::Claude => Some(claude(parent, home, cwd, resume, plain)),
        AgentKind::Codex => codex(home, cwd, resume),
        AgentKind::Gemini | AgentKind::Kimi => None,
    }?;
    Some(HelperStartArgs { model: Some(model), ..a.clone() })
}

/// A model taken over from another session (the parent pane, the
/// account's last one), once the allocation chose a plain model for
/// this helper: no Fable, and no 1M context it did not price.
pub(super) fn inherited(model: Option<String>, plain: bool) -> Option<String> {
    let m = model?;
    if !plain {
        return Some(m);
    }
    if m.contains("fable") {
        return None;
    }
    Some(m.strip_suffix("[1m]").map(str::to_string).unwrap_or(m))
}

fn claude(parent: u64, home: &Path, cwd: &str, resume: bool, plain: bool) -> String {
    let last = || {
        let p = resume.then(|| newest_transcript(home, cwd)).flatten()?;
        launch_model::session_model(p.file_stem()?.to_str()?, Some(&p))
    };
    let (model, from) = launch_model::first_known(&[
        (Source::Session, &last),
        (Source::Pane, &|| inherited(launch_model::pane_model(parent), plain)),
        (Source::Account, &|| inherited(launch_model::account_model(home), plain)),
    ]);
    let why = if from == Source::Default { "; nothing about this helper's model was ever observed" } else { "" };
    marspot::lx_info!("helpers.model_from", &format!("parent={parent} {model} ({}){why}", from.as_str()));
    model
}

fn codex(home: &Path, cwd: &str, resume: bool) -> Option<String> {
    use crate::plugins::codex::{last_model, newest_rollout_for_cwd};
    let resumed = resume.then(|| newest_rollout_for_cwd(home, cwd)).flatten().and_then(|p| last_model::rollout_model(&p));
    let model = resumed.or_else(|| last_model::account_model(home));
    match &model {
        Some(m) => marspot::lx_info!("helpers.model_from", &format!("codex {m} (observed)")),
        None => marspot::lx_info!("helpers.model_from", "codex: no model observed on this account; it starts on its config's"),
    }
    model
}

/// The newest conversation claude under `home` keeps for `cwd`.
fn newest_transcript(home: &Path, cwd: &str) -> Option<std::path::PathBuf> {
    let dir = home.join("projects").join(super::launch::claude_project_dir(cwd));
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::control::Isolation;

    fn args(agent: AgentKind, model: Option<&str>, resume_last: bool) -> HelperStartArgs {
        HelperStartArgs {
            agent,
            task: "t".into(),
            cwd: None,
            parent: None,
            model: model.map(String::from),
            profile: None,
            isolation: Isolation::Host,
            mode: None,
            resume_last,
            budget_minutes: 30,
            name: None,
            title: None,
        }
    }

    fn filled(a: &HelperStartArgs, home: &Path, resume: bool) -> Option<String> {
        late(a, 999_999, home, "/w/proj", resume, false).and_then(|a| a.model)
    }

    /// A helper's model is named whatever was asked: the conversation it
    /// goes on with, the account's last observed model, the default; and
    /// the script starts claude on it.
    #[test]
    fn helpers_claude_helper_always_names_its_model() {
        let state = marspot_term::test_env::sandbox_state_dir();
        let home = marspot_term::test_env::TempDir::new("helper-model-home");
        let fresh = args(AgentKind::Claude, None, false);
        assert_eq!(late(&args(AgentKind::Claude, Some("sonnet"), false), 1, home.path(), "/w", false, false), None, "asked is kept");
        assert_eq!(early(&args(AgentKind::Claude, Some("sonnet"), false), 1, false), None);
        assert_eq!(early(&fresh, 999_999, false), None, "a parent with no claude names nothing");
        assert_eq!(filled(&fresh, home.path(), false).as_deref(), Some(launch_model::DEFAULT_MODEL));

        let push = state.path().join("plugins/claudecode/model");
        std::fs::create_dir_all(&push).unwrap();
        let t = home.path().join("projects/-w-proj/u-1.jsonl");
        std::fs::write(push.join("u-1"), format!("x\n{}\nhigh\npids=1\nid=claude-opus-5-5\n", t.display())).unwrap();
        assert_eq!(filled(&fresh, home.path(), false).as_deref(), Some("claude-opus-5-5"), "the account's");

        let t2 = home.path().join("projects/-w-proj/u-2.jsonl");
        std::fs::create_dir_all(t2.parent().unwrap()).unwrap();
        std::fs::write(&t2, r#"{"message":{"role":"assistant","model":"claude-fable-5-1"}}"#).unwrap();
        let again = args(AgentKind::Claude, None, true);
        assert_eq!(filled(&again, home.path(), true).as_deref(), Some("claude-fable-5-1"), "the conversation's");
        assert_eq!(filled(&again, home.path(), false).as_deref(), Some("claude-opus-5-5"), "no conversation to go on with");
        // the conversation's hook record keeps the 1M context its answers leave out
        std::fs::write(push.join("u-2"), format!("x\n{}\nhigh\npids=1\nid=claude-fable-5-1[1m]\n", t2.display())).unwrap();
        assert_eq!(filled(&again, home.path(), true).as_deref(), Some("claude-fable-5-1[1m]"), "the record before the transcript");

        std::fs::write(push.join("u-2"), format!("x\n{}\nhigh\npids=1\nid=claude-opus-5-5\n", t2.display())).unwrap();
        let a = late(&fresh, 999_999, home.path(), "/w/proj", false, false).unwrap();
        let acct = super::super::launch::test_account(&home.path().to_string_lossy());
        let task = home.path().join("task.md");
        let line = super::super::launch::run_script(AgentKind::Claude, &acct, a.model.as_deref(), &[], false, &task).unwrap();
        assert!(line.contains("--model claude-opus-5-5 --"), "{line}");
        assert!(super::super::launch::run_script(AgentKind::Claude, &acct, None, &[], false, &task).is_err(), "no claude line without a model");
    }

    /// Once the allocation chose a plain model for a helper, it does not
    /// take a Fable model or a 1M context over from its parent or its
    /// account; without that choice it does.
    #[test]
    fn helpers_a_plain_pick_inherits_no_fable_and_no_wide_context() {
        let fable = || Some("claude-fable-5-1[1m]".to_string());
        let wide = || Some("claude-opus-5-5[1m]".to_string());
        assert_eq!(inherited(fable(), false).as_deref(), Some("claude-fable-5-1[1m]"));
        assert_eq!(inherited(wide(), false).as_deref(), Some("claude-opus-5-5[1m]"));
        assert_eq!(inherited(fable(), true), None);
        assert_eq!(inherited(wide(), true).as_deref(), Some("claude-opus-5-5"));

        let state = marspot_term::test_env::sandbox_state_dir();
        let home = marspot_term::test_env::TempDir::new("helper-model-plain");
        let push = state.path().join("plugins/claudecode/model");
        std::fs::create_dir_all(&push).unwrap();
        let t = home.path().join("projects/-w/u-9.jsonl");
        std::fs::write(push.join("u-9"), format!("x\n{}\nhigh\npids=1\nid=claude-fable-5-1[1m]\n", t.display())).unwrap();
        let fresh = args(AgentKind::Claude, None, false);
        let pick = |plain| late(&fresh, 999_999, home.path(), "/w", false, plain).and_then(|a| a.model);
        assert_eq!(pick(false).as_deref(), Some("claude-fable-5-1[1m]"), "no cost decision: the account's");
        assert_eq!(pick(true).as_deref(), Some(launch_model::DEFAULT_MODEL), "a plain pick passes the Fable over");
    }

    /// A codex helper names the model its rollouts last recorded.
    #[test]
    fn helpers_codex_helper_names_the_model_it_last_ran() {
        let home = marspot_term::test_env::TempDir::new("helper-model-codex");
        let fresh = args(AgentKind::Codex, None, false);
        assert_eq!(filled(&fresh, home.path(), false), None, "nothing observed");
        let day = home.path().join("sessions/2026/11/27");
        std::fs::create_dir_all(&day).unwrap();
        let turn = "{\"type\":\"turn_context\",\"payload\":{\"cwd\":\"/w\",\"model\":\"gpt-5.5\"}}\n";
        std::fs::write(day.join("rollout-1.jsonl"), turn).unwrap();
        assert_eq!(filled(&fresh, home.path(), false).as_deref(), Some("gpt-5.5"));
        assert_eq!(late(&args(AgentKind::Gemini, None, false), 1, home.path(), "/w", false, false), None, "gemini is left as it was");
    }
}
