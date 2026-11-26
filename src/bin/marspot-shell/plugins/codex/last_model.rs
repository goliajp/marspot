//! The model a codex was last seen running, for a launch that names it
//! with `-m`.
//!
//! Every account's `config.toml` is a link to one shared file, and a
//! model picked in any session can be written there; a codex started
//! without `-m` takes whatever that file says at that moment. So a
//! launch names the model its rollouts last recorded.

use std::path::Path;

use super::rollout::{json_str_field, last_fact_line, recent_rollouts, tail_of};

/// How many of an account's newest rollouts are asked for a model.
const NEWEST: usize = 8;

/// The model the last turn or pick in rollout `path` names.
pub(crate) fn rollout_model(path: &Path) -> Option<String> {
    let tail = tail_of(path)?;
    json_str_field(last_fact_line(&tail)?, "model").filter(|m| !m.is_empty())
}

/// The model the account under `home` last ran: the newest of its
/// recent rollouts that names one.
pub(crate) fn account_model(home: &Path) -> Option<String> {
    recent_rollouts(home).iter().take(NEWEST).find_map(|p| rollout_model(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn and a later pick: the pick is what the next start runs.
    #[test]
    fn codex_launch_model_is_the_last_one_its_rollouts_name() {
        let home = marspot_term::test_env::TempDir::new("codex-last-model");
        let day = home.path().join("sessions/2026/11/27");
        std::fs::create_dir_all(&day).unwrap();
        let turn = r#"{"type":"turn_context","payload":{"cwd":"/w","model":"gpt-5.5","effort":"high"}}"#;
        let pick = r#"{"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":{"cwd":"/w","model":"gpt-5.5-mini","reasoning_effort":"low"}}}"#;
        let p = day.join("rollout-1.jsonl");
        std::fs::write(&p, format!("{turn}\n{pick}\n")).unwrap();
        assert_eq!(rollout_model(&p).as_deref(), Some("gpt-5.5-mini"));
        assert_eq!(account_model(home.path()).as_deref(), Some("gpt-5.5-mini"));
        std::fs::write(&p, "{\"type\":\"session_meta\"}\n").unwrap();
        assert_eq!(account_model(home.path()), None, "nothing observed, nothing named");
    }
}
