//! A three-position setting for an agent whose accounts the shared
//! quota allocation is still being proved on, and how the keys that
//! decide what marspot does to the agents' accounts and CLIs are read
//! and written.

use super::values::parse_bool;
use super::{Rebalance, Settings};

/// What an agent's plugin does with what the allocation decides for
/// its panes, beyond the moves it made before the allocation existed.
///
/// `off` keeps the plugin's moves as they were; `dryrun` also reads the
/// extra windows and logs each move the allocation would make; `live`
/// acts on them.  Off unless the file says otherwise: such an agent's account limits
/// and refusals are known only from its documentation, and every word
/// that is not `dryrun` or `live` reads as off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentAllocation {
    #[default]
    Off,
    DryRun,
    Live,
}

impl AgentAllocation {
    pub(super) fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "live" => Self::Live,
            "dryrun" => Self::DryRun,
            _ => Self::Off,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dryrun",
            Self::Live => "live",
        }
    }
}

/// The value of one of the agent keys, `None` for any other key.
pub(super) fn value_of(s: &Settings, key: &str) -> Option<String> {
    Some(match key {
        "codex.follow_updates" => s.codex_follow_updates.to_string(),
        "agents.install" => s.agents_install.to_string(),
        "claudecode.rebalance" => s.cc_rebalance.as_str().to_string(),
        "kimi.allocation" => s.kimi_allocation.as_str().to_string(),
        _ => return None,
    })
}

/// Apply `key = v` when `key` is one of the agent keys; false otherwise.
pub(super) fn parse_key(s: &mut Settings, key: &str, v: &str) -> bool {
    match key {
        "codex.follow_updates" => s.codex_follow_updates = parse_bool(v, s.codex_follow_updates),
        "agents.install" => s.agents_install = parse_bool(v, s.agents_install),
        "claudecode.rebalance" => s.cc_rebalance = Rebalance::parse(v),
        "kimi.allocation" => s.kimi_allocation = AgentAllocation::parse(v),
        _ => return false,
    }
    true
}
