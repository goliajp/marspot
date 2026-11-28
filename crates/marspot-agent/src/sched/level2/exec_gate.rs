//! When the executor may act, and when it goes back to its dry run.
//!
//! The stages: `DryRun` writes what it would do; `Unattended` acts on
//! panes nobody is at, at most `UNATTENDED_PER_DAY` switches a day;
//! `All` acts on every pane within the profile's limits.  `DryRun` moves
//! on once `DRY_RECORDS` would-switch records replayed to the same
//! decision, `DRY_NOTICES` of them notices; `Unattended` once it made
//! `UNATTENDED_DONE` switches with no rollback.  Any of the five
//! rollback conditions sends it back to `DryRun` for good: it does not
//! go live again until a person has looked (`resume`).

/// Would-switch records the dry run needs, and how many of them notices.
pub const DRY_RECORDS: u32 = 10;
pub const DRY_NOTICES: u32 = 3;
/// Switches a day while only unattended panes are switched, and how
/// many done without a rollback open every pane.
pub const UNATTENDED_PER_DAY: u32 = 4;
pub const UNATTENDED_DONE: u32 = 8;
/// A switched pane works again within this, seconds.
pub const RESUME_S: i64 = 120;
/// A notice is answered (switched or told to continue) within this, seconds.
pub const NOTICE_ANSWER_S: i64 = 1800;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    DryRun,
    Unattended,
    All,
}

/// Why the executor went back to its dry run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rollback {
    /// A switched pane did not work again within `RESUME_S`.
    NoResume,
    /// A pane was switched twice inside its stay.
    TwiceInStay,
    /// A switch over a running turn, or over background work no notice
    /// had warned of.
    Unsafe,
    /// More refusals the plan did not foresee in a day than in the day
    /// before going live.
    Refusals,
    /// A notice went unanswered for `NOTICE_ANSWER_S`.
    Unanswered,
}

impl Rollback {
    pub fn word(self) -> &'static str {
        match self {
            Rollback::NoResume => "no_resume",
            Rollback::TwiceInStay => "twice_in_stay",
            Rollback::Unsafe => "unsafe",
            Rollback::Refusals => "refusals",
            Rollback::Unanswered => "unanswered",
        }
    }
}

/// One switch the executor made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    pub sid: u64,
    pub at: i64,
    /// When the pane worked again, if it has.
    pub resumed_at: Option<i64>,
    pub turn_running: bool,
    pub live_unnoticed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gate {
    pub stage: Stage,
    pub rolled_back: Option<Rollback>,
    dry: u32,
    dry_notices: u32,
    done_live: u32,
    /// Unforeseen refusals in the day before going live.
    pub refusals_before: u32,
}

impl Gate {
    /// A would-switch record of the dry run that replayed to the same
    /// decision; `notice`: it was a notice.
    pub fn dry_record(&mut self, notice: bool) {
        self.dry += 1;
        self.dry_notices += u32::from(notice);
        if self.stage == Stage::DryRun && self.rolled_back.is_none() && self.dry >= DRY_RECORDS && self.dry_notices >= DRY_NOTICES {
            self.stage = Stage::Unattended;
        }
    }

    /// The switches made so far and the facts around them, at `now`: the
    /// rollback that holds, if any, and the stage after it.
    pub fn check(&mut self, done: &[Done], stay_s: i64, notices: &[(i64, bool)], unforeseen_24h: u32, now: i64) -> Option<Rollback> {
        if self.stage == Stage::DryRun {
            return None;
        }
        let hit = done.iter().find_map(|d| {
            if d.turn_running || d.live_unnoticed {
                return Some(Rollback::Unsafe);
            }
            let late = d.resumed_at.map_or(now - d.at > RESUME_S, |r| r - d.at > RESUME_S);
            late.then_some(Rollback::NoResume)
        });
        let twice = || done.iter().any(|a| done.iter().any(|b| a.sid == b.sid && b.at > a.at && b.at - a.at < stay_s));
        let unanswered = || notices.iter().any(|(at, answered)| !answered && now - at > NOTICE_ANSWER_S);
        let why = hit
            .or_else(|| twice().then_some(Rollback::TwiceInStay))
            .or_else(|| (unforeseen_24h > self.refusals_before).then_some(Rollback::Refusals))
            .or_else(|| unanswered().then_some(Rollback::Unanswered));
        if let Some(w) = why {
            self.stage = Stage::DryRun;
            self.rolled_back = Some(w);
            return Some(w);
        }
        self.done_live = done.len() as u32;
        if self.stage == Stage::Unattended && self.done_live >= UNATTENDED_DONE {
            self.stage = Stage::All;
        }
        None
    }

    /// A person looked at the rollback: the dry run counts afresh.
    pub fn resume(&mut self) {
        *self = Gate { refusals_before: self.refusals_before, ..Gate::default() };
    }
}

#[cfg(test)]
#[path = "exec_gate_tests.rs"]
mod tests;
