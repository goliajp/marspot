//! A three-position setting for an agent whose accounts the shared
//! quota allocation is still being proved on, and how the keys that
//! decide what marspot does to the agents' accounts and CLIs are read
//! and written.

use super::values::parse_bool;
use super::{CcModel, Rebalance, Settings};

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
        "claude.model" => s.cc_model.as_str().to_string(),
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
        "claude.model" => s.cc_model = CcModel::parse(v),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::*;

    /// Following codex updates is on unless the file says otherwise,
    /// and saying otherwise survives a rewrite.
    #[test]
    fn codex_updates_are_followed_unless_switched_off() {
        assert!(Settings::default().codex_follow_updates);
        let off = parse("codex.follow_updates = false\n");
        assert!(!off.codex_follow_updates);
        let kept = parse(&render(&off, "codex.follow_updates = false\n"));
        assert!(!kept.codex_follow_updates, "a rewrite keeps it off");
    }

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

    /// The kimi allocation is off unless the file says `dryrun` or
    /// `live`, and a rewrite keeps the word.
    #[test]
    fn kimi_allocation_is_off_unless_named() {
        assert_eq!(Settings::default().kimi_allocation, AgentAllocation::Off);
        for (word, want) in [
            ("live", AgentAllocation::Live),
            ("DryRun", AgentAllocation::DryRun),
            ("off", AgentAllocation::Off),
            ("on", AgentAllocation::Off),
            ("true", AgentAllocation::Off),
        ] {
            let body = format!("kimi.allocation = {word}\n");
            let s = parse(&body);
            assert_eq!(s.kimi_allocation, want, "{word}");
            assert_eq!(parse(&render(&s, &body)).kimi_allocation, want, "{word} after a rewrite");
        }
        assert_eq!(value_of(&parse("kimi.allocation = dryrun\n"), "kimi.allocation"), "dryrun");
    }

    /// The reset switch is off unless the file says otherwise, and reads
    /// back as written; the retired screen-read switch is no setting.
    #[test]
    fn the_reset_switch_is_off_unless_written() {
        assert_eq!(Settings::default().sched_reset, AgentAllocation::Off);
        let body = "sched.reset = dryrun\n";
        let s = parse(body);
        assert_eq!(s.sched_reset, AgentAllocation::DryRun);
        assert_eq!(parse(&render(&s, body)), s);
        assert_eq!(value_of(&s, "sched.reset"), "dryrun");
        assert_eq!(parse("sched.reset = on\n").sched_reset, AgentAllocation::Off);
        assert_eq!(parse("sched.reset_read = live\n"), Settings::default(), "an old line changes nothing");
    }
}
