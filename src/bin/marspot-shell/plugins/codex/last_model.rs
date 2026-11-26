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

/// The model a resume of `thread` under `home` names with `-m`: what
/// the thread's rollout last recorded, else what the account last ran.
///
/// `codex resume` takes the model from `config.toml`, not from the
/// thread (it says "recorded with model … but is resuming with …"), so
/// a resume without `-m` runs whatever the shared file says. One
/// listing of the home answers both: the thread is matched by file name
/// among the rollouts newest first, and the account's model comes from
/// the same list.
pub(crate) fn thread_model(home: &Path, thread: &str) -> Option<String> {
    let all = recent_rollouts(home);
    let suffix = format!("-{thread}.jsonl");
    let own = all.iter().find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)));
    own.and_then(|p| rollout_model(p))
        .or_else(|| all.iter().take(NEWEST).find_map(|p| rollout_model(p)))
        .filter(|m| m.chars().all(|c| c.is_ascii_alphanumeric() || "-._:/".contains(c)))
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
        // a thread's own rollout first, the account's newest after it
        let thread = "01a133f4-8e7b-7331-9630-9c40ff4b467f";
        let own = day.join(format!("rollout-2026-11-27T01-00-00-{thread}.jsonl"));
        std::fs::write(&own, format!("{turn}\n")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, format!("{turn}\n{pick}\n")).unwrap();
        assert_eq!(thread_model(home.path(), thread).as_deref(), Some("gpt-5.5"));
        assert_eq!(thread_model(home.path(), "0000").as_deref(), Some("gpt-5.5-mini"), "no rollout: the account's");
        std::fs::remove_file(&own).unwrap();
        std::fs::write(&p, "{\"type\":\"session_meta\"}\n").unwrap();
        assert_eq!(thread_model(home.path(), thread), None);
        assert_eq!(account_model(home.path()), None, "nothing observed, nothing named");
    }
}
