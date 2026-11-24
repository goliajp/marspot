//! The one setting with three positions.

/// What the claudecode plugin does with an idle pane that has a better
/// account to be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebalance {
    Off,
    DryRun,
    On,
}

impl Rebalance {
    pub(super) fn parse(v: &str, fallback: Self) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "on" | "true" => Self::On,
            "dryrun" | "dry_run" | "dry-run" => Self::DryRun,
            "off" | "false" => Self::Off,
            _ => fallback,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dryrun",
            Self::On => "on",
        }
    }
}
