//! The badge-menu tags of the handoff rows.

use super::transcript::Agent;

/// Badge-menu tags the handoff rows own: `TAG_BASE | agent << 8 | profile`.
/// Well above the `u8` range the profile-cycle rows use, so the two
/// kinds of row cannot be mistaken for each other.
const TAG_BASE: u32 = 0x4F48_0000;

fn code(a: Agent) -> u32 {
    match a {
        Agent::Claude => 1,
        Agent::Codex => 2,
        Agent::Gemini => 3,
        Agent::Kimi => 4,
    }
}

pub fn tag_for(to: Agent, profile: u8) -> u32 {
    TAG_BASE | code(to) << 8 | profile as u32
}

/// The target a tag names, if it is one of ours.
pub fn parse_tag(tag: u32) -> Option<(Agent, u8)> {
    if tag & 0xFFFF_0000 != TAG_BASE {
        return None;
    }
    let agent = match (tag >> 8) & 0xFF {
        1 => Agent::Claude,
        2 => Agent::Codex,
        3 => Agent::Gemini,
        4 => Agent::Kimi,
        _ => return None,
    };
    Some((agent, (tag & 0xFF) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_stay_clear_of_profile_rows() {
        for (a, n) in [(Agent::Claude, 1u8), (Agent::Codex, 7), (Agent::Codex, 255), (Agent::Gemini, 0), (Agent::Gemini, 2), (Agent::Kimi, 0), (Agent::Kimi, 3)] {
            assert_eq!(parse_tag(tag_for(a, n)), Some((a, n)));
            assert!(u8::try_from(tag_for(a, n)).is_err(), "a profile-cycle plugin must ignore it");
        }
        assert_eq!(parse_tag(3), None);
        assert_eq!(parse_tag(tag_for(Agent::Kimi, 1) + 0x100), None, "an agent number nobody has");
    }
}
