//! What the sweep keeps across restarts: the heads-up ledger and the
//! refusals seen.

use super::super::*;

const HEADS_UP_FILE: &str = "heads_up.tsv";
const REFUSED_FILE: &str = "refused.tsv";

impl ClaudecodePlugin {
    pub(in crate::plugins::claudecode) fn load_heads_up(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join(HEADS_UP_FILE)) {
            self.heads_up = heads_up::Ledger::decode(&text);
        }
    }

    pub(in crate::plugins::claudecode) fn load_refused(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join(REFUSED_FILE)) {
            self.refused = seen::Seen::decode(&text);
        }
    }

    pub(super) fn persist_refused(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.refused.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = fs::write(dir.join(REFUSED_FILE), text) {
            host.log(LogLevel::Warn, "quota.refused_persist_failed", &format!("{e}"));
        }
    }

    pub(super) fn persist_heads_up(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.heads_up.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = fs::write(dir.join(HEADS_UP_FILE), text) {
            host.log(LogLevel::Warn, "quota.heads_up_persist_failed", &format!("{e}"));
        }
    }
}
