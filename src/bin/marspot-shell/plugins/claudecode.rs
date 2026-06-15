//! marspot-claudecode plugin — first marspot plugin.
//!
//! Detects which claudecode session each pane is running and surfaces
//! the sessionId + conversation status to the host.
//!
//! ## Detection pipeline (per pane, per tick)
//!
//! 1. host.pane_pty_pid_tree(pane) → walk children for a `claude` /
//!    `node ... claude` process
//! 2. that process's `cwd` → encode to `~/.claude/projects/<enc>/`
//! 3. find newest `.jsonl` in that dir → parse first line, extract
//!    `sessionId`
//! 4. tail the file by size delta → look at last record's `type`
//!    to infer "thinking" / "tool_use" / "waiting_user" / "done"
//!
//! Milestone 1 (this file): registration skeleton + tick stub.
//! Milestone 2: full detection pipeline + status widget hookup.

use crate::plugins::{Plugin, PluginError, PluginHost, PluginMetadata, PermissionSet, PLUGIN_API_VERSION, LogLevel};

pub struct ClaudecodePlugin {
    /// Reserved for per-pane state cache (Milestone 2).
    initialised: bool,
}

impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self { initialised: false }
    }
}

impl Default for ClaudecodePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ClaudecodePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "claudecode",
            version: "0.1.0",
            api_version: PLUGIN_API_VERSION,
            permissions: PermissionSet::READ_PANE_INFO
                | PermissionSet::READ_PTY_TREE
                | PermissionSet::READ_DISK_FS
                | PermissionSet::NOTIFY_USER
                | PermissionSet::SET_STATUS_LINE
                | PermissionSet::PERSIST_STATE,
            tick_interval_ms: 500,
        }
    }

    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // Phase-1: just verify our permission gate works end-to-end.
        // Phase-2: load persisted per-pane sessionId cache from
        // host.state_dir() so a restart picks up where it left off.
        let _ = host.state_dir()?;
        host.log(LogLevel::Info, "init", "claudecode plugin initialised");
        self.initialised = true;
        Ok(())
    }

    fn start(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        host.log(LogLevel::Info, "start", "plugin started");
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        // Milestone 1: prove the tick fires, count panes.  Milestone 2
        // walks pid_tree + parses .jsonl + emits status_widget /
        // notify on assistant-message arrival.
        let count = host.pane_count();
        host.log(
            LogLevel::Debug,
            "tick",
            &format!("tick ({} panes)", count),
        );
    }

    fn stop(&mut self, host: &dyn PluginHost) {
        host.log(LogLevel::Info, "stop", "plugin stopped");
        self.initialised = false;
    }
}
