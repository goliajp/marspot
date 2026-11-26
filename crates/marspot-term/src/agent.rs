//! `AgentKind`: which agent CLI a pane or a helper runs.
//!
//! The numbers are wire values, read by builds older and newer than this
//! one: a value is never renumbered or reused.  0 on the wire means no
//! agent, or one none of these is, and has no variant here: `Option<AgentKind>`
//! carries it.

use crate::shell_proto::StripAgent;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude = 1,
    Codex = 2,
    Gemini = 3,
    Kimi = 4,
}

impl AgentKind {
    pub const ALL: [AgentKind; 4] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini, AgentKind::Kimi];

    pub fn wire(self) -> u8 {
        self as u8
    }

    /// None for 0 and for a value from a later build.
    pub fn from_wire(b: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.wire() == b)
    }

    /// The name of the agent's command.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::Kimi => "kimi",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl From<AgentKind> for StripAgent {
    fn from(k: AgentKind) -> Self {
        StripAgent::from_u8(k.wire())
    }
}

impl From<Option<AgentKind>> for StripAgent {
    fn from(k: Option<AgentKind>) -> Self {
        k.map_or(StripAgent::Other, StripAgent::from)
    }
}

impl StripAgent {
    /// None for `Other` and for a value this build does not know.
    pub fn kind(self) -> Option<AgentKind> {
        AgentKind::from_wire(self.to_u8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kind_wire_values_are_frozen() {
        let pinned = [(AgentKind::Claude, 1), (AgentKind::Codex, 2), (AgentKind::Gemini, 3), (AgentKind::Kimi, 4)];
        for (k, b) in pinned {
            assert_eq!(k.wire(), b, "{k:?}");
            assert_eq!(AgentKind::from_wire(b), Some(k));
            assert_eq!(AgentKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(AgentKind::from_wire(0), None, "0 is no agent");
        assert_eq!(AgentKind::from_wire(5), None);
        assert_eq!(AgentKind::ALL.len(), pinned.len());
    }

    #[test]
    fn strip_agent_bytes_match_agent_kind() {
        let pinned = [
            (StripAgent::Other, 0, None),
            (StripAgent::Claude, 1, Some(AgentKind::Claude)),
            (StripAgent::Codex, 2, Some(AgentKind::Codex)),
            (StripAgent::Gemini, 3, Some(AgentKind::Gemini)),
            (StripAgent::Kimi, 4, Some(AgentKind::Kimi)),
        ];
        for (s, b, k) in pinned {
            assert_eq!(s.to_u8(), b, "{s:?}");
            assert_eq!(StripAgent::from_u8(b), s);
            assert_eq!(s.kind(), k);
            assert_eq!(StripAgent::from(k), s);
        }
        assert_eq!(StripAgent::Unknown(9).kind(), None);
    }
}
