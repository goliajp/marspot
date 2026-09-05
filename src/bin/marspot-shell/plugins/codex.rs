//! codex — the OpenAI CLI, given the same pane affordances claudecode
//! has (RFC-008).
//!
//! Deliberately NOT a copy of `claudecode.rs`.  What the two agents
//! share is already parameterised: the pane→process binding takes a
//! predicate (`looks_like_*`), badges go through `PluginHost`, and the
//! registry dispatches to whoever claims a session.  So this plugin
//! supplies the parts that genuinely differ — how to recognise the
//! process, and what its badge says — and inherits the rest.
//!
//! What is deliberately absent for now: claudecode's profile-cycle
//! (SIGTERM, await-quiet, relaunch with `--resume`) leans on claude's
//! session-resume semantics, and codex's equivalent has not been
//! established.  Guessing at it would put a plugin in a position to
//! kill the user's agent mid-task.

use std::path::PathBuf;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PermissionSet,
            PLUGIN_API_VERSION};
use crate::plugins::pidtree;

/// Is this descendant the `codex` CLI?
///
/// Same argv[0] approach `looks_like_claudecode` needs: a released
/// codex renames its process, so `comm` is not dependable.  The
/// basename must match exactly — `codex-code-mode-host` is a CHILD
/// helper codex spawns, and treating it as the agent would bind a
/// pane to the wrong pid and badge it twice.
pub fn looks_like_codex(d: &pidtree::ProcRow) -> bool {
    let Some(line) = pidtree::proc_cmdline(d.pid) else {
        return false;
    };
    let Some(argv0) = line.split(' ').next() else {
        return false;
    };
    argv0.rsplit('/').next().unwrap_or(argv0) == "codex"
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// `model` and `model_reasoning_effort` from `~/.codex/config.toml`.
///
/// Read as text rather than parsed: the file is TOML with per-project
/// tables, and only two top-level scalars are wanted.  Stopping at the
/// first table header keeps a `[projects."…"]` section's own keys from
/// being mistaken for the globals.
fn read_model_and_effort(home: &PathBuf) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return (None, None);
    };
    let (mut model, mut effort) = (None, None);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            break; // into per-project / per-feature tables
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "model" => model = Some(v),
            "model_reasoning_effort" => effort = Some(v),
            _ => {}
        }
    }
    (model, effort)
}

/// `gpt-6-astra·high` — the shape claudecode's badge already uses for
/// its own model and effort, so the two read as one system.
fn badge_text(model: Option<&str>, effort: Option<&str>) -> String {
    match (model, effort) {
        (Some(m), Some(e)) => format!("{m}·{e}"),
        (Some(m), None) => m.to_string(),
        (None, Some(e)) => format!("codex·{e}"),
        (None, None) => "codex".to_string(),
    }
}

pub struct CodexPlugin {
    initialised: bool,
    /// Last badge published per session, so an unchanged scan does not
    /// republish — a badge write invalidates the pane's render cache.
    last_badge: std::collections::HashMap<u64, String>,
}

impl CodexPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: std::collections::HashMap::new(),
        }
    }
}

impl Default for CodexPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CodexPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "codex",
            version: "0.1.0",
            api_version: PLUGIN_API_VERSION,
            permissions: PermissionSet::READ_PANE_INFO
                | PermissionSet::READ_PTY_TREE
                | PermissionSet::READ_DISK_FS
                | PermissionSet::SET_STATUS_LINE,
            // Matches claudecode's cadence.  The badge only moves when
            // the user changes model or effort, so anything faster
            // would be spending CPU to watch a file that rarely moves.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, _host: &dyn PluginHost) -> Result<(), PluginError> {
        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        let Some(home) = codex_home() else { return };
        let (model, effort) = read_model_and_effort(&home);
        let text = badge_text(model.as_deref(), effort.as_deref());

        // Sessions come from the registry, not from pane indices: a
        // pane index is a position in a layout that moves when panes
        // are dragged or closed, while the session id is what a badge
        // is addressed to.  claudecode's scan reads the same source.
        let procs = pidtree::list_all_procs();
        for entry in marspot_term::session_registry::list_session_entries() {
            let sid = entry.id;
            let shell = entry.shell_child_pid;
            if shell <= 0 {
                continue;
            }
            let has_codex = pidtree::descendants_of(shell, &procs)
                .iter()
                .any(looks_like_codex);
            if has_codex {
                if self.last_badge.get(&sid).map(String::as_str) != Some(text.as_str()) {
                    if host.set_pane_badge(sid, &text).is_ok() {
                        host.log(
                            LogLevel::Info,
                            "codex.badge.changed",
                            &format!("sid={sid} badge={text:?}"),
                        );
                        self.last_badge.insert(sid, text.clone());
                    }
                }
            } else if self.last_badge.remove(&sid).is_some() {
                // codex left this pane: clear the badge we set, and
                // only the one we set — another plugin may own it now.
                let _ = host.set_pane_badge(sid, "");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge reads like claudecode's, so a window with both does
    /// not look like two unrelated tools.
    #[test]
    fn badge_pairs_model_with_effort() {
        assert_eq!(badge_text(Some("gpt-6-astra"), Some("high")), "gpt-6-astra·high");
        assert_eq!(badge_text(Some("gpt-6-astra"), None), "gpt-6-astra");
        assert_eq!(badge_text(None, None), "codex");
    }

    /// Only the top-level scalars count.  `~/.codex/config.toml` carries
    /// `[projects."…"]` tables whose keys would otherwise be read as the
    /// global model, and the badge would follow whichever project was
    /// listed last rather than what codex is actually running.
    #[test]
    fn per_project_tables_do_not_leak_into_the_globals() {
        let dir = std::env::temp_dir().join(format!("codexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\
             [projects.\"/x\"]\nmodel = \"WRONG\"\n",
        )
        .unwrap();
        let got = read_model_and_effort(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(got, (Some("gpt-6-astra".into()), Some("high".into())));
    }
}
