//! The one setting with three positions.

/// What the claudecode plugin does with the moves of resting panes the
/// quota allocation decides.
///
/// Live unless the file says otherwise; `on` and `true` say the same.
/// A word that is none of the known ones reads as `DryRun`: it logs the
/// moves it would make and makes none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebalance {
    Off,
    DryRun,
    Live,
}

impl Rebalance {
    pub(super) fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "live" | "on" | "true" => Self::Live,
            "off" | "false" => Self::Off,
            // `dryrun` and anything unknown
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
    /// keeps them.  With no line it is live, as are `on` and `true`; a
    /// word that is none of them is a dry run.
    #[test]
    fn rebalance_is_live_unless_written_otherwise() {
        assert_eq!(Settings::default().cc_rebalance, Rebalance::Live);
        assert_eq!(parse("").cc_rebalance, Rebalance::Live);
        for (word, want) in [
            ("live", Rebalance::Live),
            ("dryrun", Rebalance::DryRun),
            ("off", Rebalance::Off),
            ("on", Rebalance::Live),
            ("true", Rebalance::Live),
            ("ON", Rebalance::Live),
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
