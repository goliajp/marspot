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
