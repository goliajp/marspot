//! The menu rows a pane offers to hand it to another agent.

use std::path::PathBuf;

use super::tags::handoff_tag;
use super::transcript::Agent;
use crate::plugins::{claudecode, codex, gemini, kimi};
use marspot::ui::strings::{self, Arg};

/// An agent's account profiles: number and directory.  gemini or kimi
/// with no numbered profile has its default home, as number 0.
pub fn profiles(a: Agent) -> Vec<(u8, PathBuf)> {
    match a {
        Agent::Claude => {
            let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
            claudecode::discover_profiles()
                .into_iter()
                .map(|n| (n, home.join(format!(".claude-profile-{n}"))))
                .collect()
        }
        Agent::Codex => codex::profile_dirs(),
        Agent::Gemini => gemini::handoff_homes(),
        Agent::Kimi => kimi::handoff_homes(),
    }
}

/// How a target reads in a menu row and on the badge: `codex P2`, or
/// the bare name for gemini's default home.
pub(super) fn target_name(to: Agent, profile: u8) -> String {
    match (to, profile) {
        (Agent::Gemini | Agent::Kimi, 0) => to.key().to_string(),
        _ => format!("{} P{profile}", to.key()),
    }
}

/// The rows a pane running `from` offers: one per profile of each other
/// agent.
pub fn menu_rows(from: Agent) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
    let rows = |to: Agent| {
        profiles(to).into_iter().map(move |(n, _)| marspot::shell_proto::PaneBadgeMenuItem {
            tag: handoff_tag(from, to, n),
            label: strings::text(Arg::HandOffTo(&target_name(to, n))),
        })
    };
    from.others().into_iter().flat_map(rows).collect()
}
