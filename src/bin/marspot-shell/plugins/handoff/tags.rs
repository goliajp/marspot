//! The badge-menu tags of the agent plugins' rows.
//!
//! Every plugin hears every pick (the registry fans it out), so a tag
//! has to say whose row it is.  Profile rows used to be the bare profile
//! number in all three of claude, codex and gemini: on 2026-11-17 a pick
//! of "P2" on claude's badge was read by the codex plugin as "switch
//! this pane's codex to profile 2", and a codex resume line was typed
//! into claude.  A tag now carries the plugin whose menu the row is in,
//! and [`parse_tag`] gives a plugin only its own rows.
//!
//! `TAG_BASE | owner << 12 | target << 8 | profile`: `target` is 0 for
//! the owner's own profile rows and the receiving agent for a handoff.

use super::transcript::Agent;

/// Well above the `u8` range of the rows of other plugins and the
/// core's own menu tags.
const TAG_BASE: u32 = 0x4F48_0000;

fn code(a: Agent) -> u32 {
    match a {
        Agent::Claude => 1,
        Agent::Codex => 2,
        Agent::Gemini => 3,
        Agent::Kimi => 4,
    }
}

fn agent(code: u32) -> Option<Agent> {
    match code {
        1 => Some(Agent::Claude),
        2 => Some(Agent::Codex),
        3 => Some(Agent::Gemini),
        4 => Some(Agent::Kimi),
        _ => None,
    }
}

/// What a row of `owner`'s menu asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Move the pane's own agent to this profile.
    Profile(u8),
    /// Hand the conversation to this agent, under this profile.
    HandOff(Agent, u8),
}

/// The tag of `owner`'s row moving its agent to `profile`.
pub fn profile_tag(owner: Agent, profile: u8) -> u32 {
    TAG_BASE | code(owner) << 12 | profile as u32
}

/// The tag of `owner`'s row handing the conversation to `to`.
pub fn handoff_tag(owner: Agent, to: Agent, profile: u8) -> u32 {
    TAG_BASE | code(owner) << 12 | code(to) << 8 | profile as u32
}

/// What `tag` asks of `owner`, or None when it is not a row of
/// `owner`'s menu: another plugin's, or nobody's.
pub fn parse_tag(owner: Agent, tag: u32) -> Option<Pick> {
    if tag & 0xFFFF_0000 != TAG_BASE || (tag >> 12) & 0xF != code(owner) {
        return None;
    }
    let profile = (tag & 0xFF) as u8;
    match (tag >> 8) & 0xF {
        0 => Some(Pick::Profile(profile)),
        to => agent(to).filter(|to| *to != owner).map(|to| Pick::HandOff(to, profile)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Gemini, Agent::Kimi];

    #[test]
    fn a_tag_reads_back_only_for_the_plugin_whose_row_it_is() {
        for owner in ALL {
            for n in [0u8, 1, 2, 7, 255] {
                assert_eq!(parse_tag(owner, profile_tag(owner, n)), Some(Pick::Profile(n)));
                for to in ALL.into_iter().filter(|a| *a != owner) {
                    assert_eq!(parse_tag(owner, handoff_tag(owner, to, n)), Some(Pick::HandOff(to, n)));
                }
                for other in ALL.into_iter().filter(|a| *a != owner) {
                    assert_eq!(parse_tag(other, profile_tag(owner, n)), None, "{owner:?} P{n} read by {other:?}");
                    for to in ALL {
                        assert_eq!(parse_tag(other, handoff_tag(owner, to, n)), None);
                    }
                }
            }
        }
    }

    #[test]
    fn nothing_else_reads_as_a_row() {
        for owner in ALL {
            for tag in [0, 2, 255, 0x4000_0000, 0x4F48_0000, 0x4F48_5002, profile_tag(owner, 1) ^ 0x0100_0000] {
                assert_eq!(parse_tag(owner, tag), None, "{owner:?} {tag:#x}");
            }
            assert_eq!(parse_tag(owner, handoff_tag(owner, owner, 1)), None, "a handoff to itself is no row");
        }
    }
}
