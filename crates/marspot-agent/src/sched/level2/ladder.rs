//! The steps by which the planner goes live, each entered on counted
//! events, never on time served alone:
//!
//! - `Shadow`: the routing table and the executor only write records.
//! - `A`: the routing table places helpers, refused panes and badge
//!   clicks.  Entered once `ROUTE_RECORDS` shadow placements in a row
//!   put no other-model newcomer on an account with nothing but
//!   Fable-usable quota left.  Within a day of going live, two panes it
//!   placed refused within an hour of their placement send the ladder
//!   back to `Shadow`.
//! - `B`: the executor's dry run, writing what it would switch and
//!   notify.  Entered once `A` is live.
//! - `C`: the executor switches unattended panes, at most
//!   `UNATTENDED_PER_DAY` a day.  Entered once `DRY_RECORDS` rehearsed
//!   switches in a row replayed to the decision recorded with them,
//!   `DRY_NOTICES` of them switches after a notice; a record that does
//!   not replay starts the count again.
//! - `D`: every pane, within the profile's limits.  Entered after
//!   `UNATTENDED_DONE` switches at `C` with no rollback.
//! - `E`: the live plan protects what only Fable can use.  Entered after
//!   a day at `D`.  Making unattended panes wait stays a rehearsal; one
//!   that would make an attended pane wait is recorded.
//!
//! Every advance is capped by the caller (`LADDER_MAX` in this build):
//! the steps above it are kept here, tested, and not entered.  Any of
//! the five rollback conditions (`exec_gate::rollback`) at `C`, `D` or
//! `E` sends the ladder back to `B`.  A ladder that rolled back, or
//! whose file could not be read, does not advance again until a person
//! has looked (`reset`).

use super::exec_gate::{DRY_NOTICES, DRY_RECORDS, Done, Rollback, UNATTENDED_DONE, rollback};

#[path = "ladder_text.rs"]
mod text;

/// The highest step this build enters.  The executor stays a dry run
/// until a later build raises it.
pub const LADDER_MAX: Step = Step::B;
/// Shadow placements in a row that `A` needs.
pub const ROUTE_RECORDS: u32 = 20;
/// `A`'s rollback counts refusals in its first day, of panes placed
/// within this long before, seconds.
pub const PLACED_WINDOW_S: i64 = 3600;
pub const PLACED_REFUSED: usize = 2;
const DAY_S: i64 = 86_400;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    #[default]
    Shadow,
    A,
    B,
    C,
    D,
    E,
}

const STEPS: [Step; 6] = [Step::Shadow, Step::A, Step::B, Step::C, Step::D, Step::E];

impl Step {
    pub fn word(self) -> &'static str {
        match self {
            Step::Shadow => "shadow",
            Step::A => "a",
            Step::B => "b",
            Step::C => "c",
            Step::D => "d",
            Step::E => "e",
        }
    }

    pub fn of(word: &str) -> Option<Step> {
        STEPS.into_iter().find(|s| s.word() == word)
    }
}

/// Why a step was rolled back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    PlacedRefused,
    Exec(Rollback),
    /// The kept ladder could not be read: whatever it held, it may have
    /// held a rollback.
    Unreadable,
}

impl Why {
    pub fn word(self) -> &'static str {
        match self {
            Why::PlacedRefused => "placed_refused",
            Why::Exec(r) => r.word(),
            Why::Unreadable => "unreadable",
        }
    }

    pub fn of(word: &str) -> Option<Why> {
        match word {
            "placed_refused" => Some(Why::PlacedRefused),
            "unreadable" => Some(Why::Unreadable),
            _ => Rollback::of(word).map(Why::Exec),
        }
    }
}

/// One move of the ladder: an advance (`why` none) or a rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub from: Step,
    pub to: Step,
    pub why: Option<Why>,
}

/// Which panes a live executor may switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Unattended,
    All,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ladder {
    pub step: Step,
    /// When the step was entered.
    pub since: i64,
    /// When `A` went live.
    pub a_at: Option<i64>,
    /// The step rolled back from and why; no advance while set.
    pub rolled: Option<(Step, Why)>,
    /// Shadow placements since the last one that went wrong, and those
    /// that did.
    pub route_run: u32,
    pub route_bad: u32,
    /// Refusals of panes `A` placed, in its first day.
    pub placed_refused: Vec<i64>,
    /// Rehearsed switches in a row that replayed to their recorded
    /// decision, those after a notice among them, and the records that
    /// did not replay.
    pub dry: u32,
    pub dry_notices: u32,
    pub dry_mismatch: u32,
    /// Switches made live since `C`.
    pub done: u32,
    /// Refusals the plan did not foresee in the day before `C`.
    pub refusals_before: u32,
    /// A rehearsed wait would have made an attended pane wait.
    pub attended_wait: bool,
    /// The person's last reset (`reset`), as they stamped it.
    pub reset_at: i64,
}

impl Ladder {
    /// The ladder put in place of a kept one that could not be read.
    pub fn unreadable(now: i64) -> Ladder {
        Ladder { since: now, rolled: Some((Step::Shadow, Why::Unreadable)), ..Ladder::default() }
    }

    fn go(&mut self, to: Step, why: Option<Why>, now: i64) -> Option<Change> {
        let from = self.step;
        self.step = to;
        self.since = now;
        if let Some(w) = why {
            self.rolled = Some((from, w));
        }
        Some(Change { from, to, why })
    }

    /// A shadow placement; `bad`: it put an other-model newcomer on an
    /// account with nothing but Fable-usable quota left.  Advances no
    /// higher than `max`.
    pub fn routed(&mut self, bad: bool, now: i64, max: Step) -> Option<Change> {
        self.route_bad += u32::from(bad);
        self.route_run = if bad { 0 } else { self.route_run.saturating_add(1) };
        if self.step == Step::Shadow && max >= Step::A && self.rolled.is_none() && self.route_run >= ROUTE_RECORDS {
            self.a_at = Some(now);
            return self.go(Step::A, None, now);
        }
        None
    }

    /// A pane the table placed live at `placed_at` was refused at `at`.
    pub fn placed_refused(&mut self, placed_at: i64, at: i64) -> Option<Change> {
        let first_day = self.a_at.is_some_and(|a| at - a < DAY_S);
        if !self.routes_live() || !first_day || at - placed_at > PLACED_WINDOW_S || self.placed_refused.len() >= 8 {
            return None;
        }
        self.placed_refused.push(at);
        if self.placed_refused.len() >= PLACED_REFUSED {
            return self.go(Step::Shadow, Some(Why::PlacedRefused), at);
        }
        None
    }

    /// A rehearsed switch; `same`: decided again from its record alone,
    /// it gave the decision recorded with it; `notice`: a switch after a
    /// notice.  A record that does not replay starts the count again.
    pub fn dry_record(&mut self, same: bool, notice: bool) {
        if self.step != Step::B {
            return;
        }
        if same {
            self.dry += 1;
            self.dry_notices += u32::from(notice);
        } else {
            self.dry_mismatch += 1;
            self.dry = 0;
            self.dry_notices = 0;
        }
    }

    /// A switch the executor made live.
    pub fn switched(&mut self, now: i64, max: Step) -> Option<Change> {
        self.executes()?;
        self.done += 1;
        if self.step == Step::C && max >= Step::D && self.done >= UNATTENDED_DONE {
            return self.go(Step::D, None, now);
        }
        None
    }

    /// The rollback conditions over the live switches and notices.
    pub fn check(&mut self, done: &[Done], stay_s: i64, notices: &[(i64, bool)], unforeseen_24h: u32, now: i64) -> Option<Change> {
        self.executes()?;
        let why = rollback(done, stay_s, notices, unforeseen_24h, self.refusals_before, now)?;
        self.go(Step::B, Some(Why::Exec(why)), now)
    }

    /// The advances that wait on nothing but the step's own state, no
    /// higher than `max`; `unforeseen_24h`: refusals the plan did not
    /// foresee in the last day.
    pub fn tick(&mut self, unforeseen_24h: u32, now: i64, max: Step) -> Option<Change> {
        if self.rolled.is_some() {
            return None;
        }
        let next = match self.step {
            Step::A => Step::B,
            Step::B if self.dry >= DRY_RECORDS && self.dry_notices >= DRY_NOTICES => Step::C,
            Step::D if now - self.since >= DAY_S => Step::E,
            _ => return None,
        };
        if next > max {
            return None;
        }
        if next == Step::C {
            self.refusals_before = unforeseen_24h;
            self.done = 0;
        }
        self.go(next, None, now)
    }

    /// A rehearsed wait would have made an attended pane wait.
    pub fn waited_attended(&mut self) {
        self.attended_wait = true;
    }

    pub fn routes_live(&self) -> bool {
        self.step >= Step::A
    }

    pub fn executes(&self) -> Option<Scope> {
        match self.step {
            Step::C => Some(Scope::Unattended),
            Step::D | Step::E => Some(Scope::All),
            _ => None,
        }
    }

    pub fn dry_run(&self) -> bool {
        self.step == Step::B
    }

    pub fn protects(&self) -> bool {
        self.step == Step::E
    }

    /// A person looked at a rollback, stamping it `stamp`: the step it
    /// fell to counts afresh.
    pub fn reset(&mut self, now: i64, stamp: i64) {
        let step = self.step;
        *self = Ladder { step, since: now, a_at: (step >= Step::A).then_some(now), reset_at: stamp, ..Ladder::default() };
    }
}

#[cfg(test)]
#[path = "ladder_tests.rs"]
mod tests;
