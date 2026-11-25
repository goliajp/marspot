//! The one setting with three positions.

/// What the claudecode plugin does with the moves of resting panes the
/// quota allocation decides.
///
/// `on` and `true` once switched on an older rule that moved idle panes
/// by renewal and kept Fable for the panes on it.  That rule is gone,
/// and a file that still says `on` was written for it, not for the
/// allocation: it reads as `DryRun`, as does any word that is none of
/// the known ones.  Only `live` makes the moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebalance {
    Off,
    DryRun,
    Live,
}

impl Rebalance {
    pub(super) fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "live" => Self::Live,
            "off" | "false" => Self::Off,
            // `dryrun`, the old `on` and `true`, and anything unknown
            _ => Self::DryRun,
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

#[cfg(test)]
mod tests {
    use super::super::*;

    /// The three rebalance modes read back as written and a rewrite
    /// keeps them.  The words an older build wrote for a rule that is
    /// gone, `on` and `true`, read as a dry run, as does a word that is
    /// none of them: only `live` moves.
    #[test]
    fn rebalance_moves_only_when_live() {
        for (word, want) in [
            ("live", Rebalance::Live),
            ("dryrun", Rebalance::DryRun),
            ("off", Rebalance::Off),
            ("on", Rebalance::DryRun),
            ("true", Rebalance::DryRun),
            ("ON", Rebalance::DryRun),
            ("false", Rebalance::Off),
            ("maybe", Rebalance::DryRun),
        ] {
            let body = format!("claudecode.rebalance = {word}\n");
            let s = parse(&body);
            assert_eq!(s.cc_rebalance, want, "{word}");
            assert_eq!(parse(&render(&s, &body)).cc_rebalance, want, "{word} after a rewrite");
        }
        assert_eq!(parse("claudecode.rebalance = off\nclaudecode.rebalance = maybe\n").cc_rebalance, Rebalance::DryRun, "unknown is a dry run, not the line before");
        assert_eq!(value_of(&parse("claudecode.rebalance = live\n"), "claudecode.rebalance"), "live");
    }
}
