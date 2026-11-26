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

    pub fn as_str(self) -> &'static str {
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
        "gemini.allocation" => s.gemini_allocation.as_str().to_string(),
        "sched.reset" => s.sched_reset.as_str().to_string(),
        "sched.reset_read" => s.sched_reset_read.as_str().to_string(),
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
        "gemini.allocation" => s.gemini_allocation = AgentAllocation::parse(v),
        "sched.reset" => s.sched_reset = AgentAllocation::parse(v),
        "sched.reset_read" => s.sched_reset_read = AgentAllocation::parse(v),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn gemini_allocation_is_off_unless_written_dryrun_or_live() {
        assert_eq!(Settings::default().gemini_allocation, AgentAllocation::Off);
        for (word, want) in [
            ("live", AgentAllocation::Live),
            ("DryRun", AgentAllocation::DryRun),
            ("off", AgentAllocation::Off),
            ("on", AgentAllocation::Off),
            ("true", AgentAllocation::Off),
            ("maybe", AgentAllocation::Off),
        ] {
            let body = format!("gemini.allocation = {word}\n");
            let s = parse(&body);
            assert_eq!(s.gemini_allocation, want, "{word}");
            assert_eq!(parse(&render(&s, &body)).gemini_allocation, want, "{word} after a rewrite");
        }
        assert_eq!(value_of(&parse("gemini.allocation = live\n"), "gemini.allocation"), "live");
    }

    /// Both reset switches are off unless the file says otherwise, and
    /// read back as written.
    #[test]
    fn the_reset_switches_are_off_unless_written() {
        let s = Settings::default();
        assert_eq!((s.sched_reset, s.sched_reset_read), (AgentAllocation::Off, AgentAllocation::Off));
        let body = "sched.reset = dryrun\nsched.reset_read = live\n";
        let s = parse(body);
        assert_eq!((s.sched_reset, s.sched_reset_read), (AgentAllocation::DryRun, AgentAllocation::Live));
        assert_eq!(parse(&render(&s, body)), s);
        assert_eq!(value_of(&s, "sched.reset_read"), "live");
        assert_eq!(parse("sched.reset = on\n").sched_reset, AgentAllocation::Off);
    }
}
