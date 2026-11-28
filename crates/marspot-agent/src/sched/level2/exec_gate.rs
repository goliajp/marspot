//! When a live executor goes back to its dry run: the five rollback
//! conditions over the switches it made and the notices it sent.  The
//! counts that let it go live are the ladder's (`ladder`).

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

/// Why the executor went back to its dry run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

    pub fn of(word: &str) -> Option<Rollback> {
        [Rollback::NoResume, Rollback::TwiceInStay, Rollback::Unsafe, Rollback::Refusals, Rollback::Unanswered].into_iter().find(|r| r.word() == word)
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

/// The rollback that holds over the switches made so far, at `now`:
/// `done` with the stay in force, the notices sent (when, and whether
/// they were answered), the refusals the plan did not foresee in the
/// last day against the day before going live.
pub fn rollback(done: &[Done], stay_s: i64, notices: &[(i64, bool)], unforeseen_24h: u32, refusals_before: u32, now: i64) -> Option<Rollback> {
    let hit = done.iter().find_map(|d| {
        if d.turn_running || d.live_unnoticed {
            return Some(Rollback::Unsafe);
        }
        let late = d.resumed_at.map_or(now - d.at > RESUME_S, |r| r - d.at > RESUME_S);
        late.then_some(Rollback::NoResume)
    });
    let twice = || done.iter().any(|a| done.iter().any(|b| a.sid == b.sid && b.at > a.at && b.at - a.at < stay_s));
    let unanswered = || notices.iter().any(|(at, answered)| !answered && now - at > NOTICE_ANSWER_S);
    hit.or_else(|| twice().then_some(Rollback::TwiceInStay))
        .or_else(|| (unforeseen_24h > refusals_before).then_some(Rollback::Refusals))
        .or_else(|| unanswered().then_some(Rollback::Unanswered))
}

#[cfg(test)]
#[path = "exec_gate_tests.rs"]
mod tests;
