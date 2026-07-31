//! Pane status as an actual state machine.
//!
//! The previous design was a sampled classifier plus a per-tick diff:
//! every sweep recomputed a label from scratch and logged the deltas.
//! That is enough to *watch* panes and not enough to *act* on them —
//! it had no notion of a legal transition, no way to express that two
//! layers disagree, no confirmation before declaring a pane quiet, and
//! it expressed "there is no claude here" as a missing map entry
//! rather than as a state.
//!
//! What this module adds:
//!
//! - **Two input alphabets, one composed state.**  [`Generic`] is the
//!   kernel's view (who owns the tty, what else is alive), [`Activity`]
//!   is a plugin's view of the program it understands.  [`compose`]
//!   folds them into [`PaneStatus`] through one documented table.
//! - **Disagreement is a state.**  A binding that says "claude is
//!   mid-turn" while the kernel says the pane has no processes at all
//!   is not silently rounded off; it becomes
//!   [`PaneStatus::Contradiction`], which no policy may act on.
//! - **Absence is a state.**  [`Activity::Absent`] means "no claude
//!   here", distinct from [`Activity::Unknown`] = "nothing has looked
//!   yet".
//! - **Asymmetric hysteresis.**  Entering a quiet state takes
//!   [`CONFIRM_TICKS`] agreeing observations; leaving it takes one.
//!   Acting on a pane that only *looked* quiet for a moment is the
//!   expensive mistake, so the machine is slow to say yes and instant
//!   to say no.
//!
//! Consumers read [`PaneMachine::quiescent`] — never the raw variants —
//! so the "may I touch this pane" rule lives in one place.

use std::time::{Duration, Instant};

use crate::pidtree::JobLeader;

/// How many consecutive agreeing observations are needed before the
/// machine will commit to a quiet state.  At the shell's 1 s sweep
/// that is ~3 s of agreement.
///
/// Transitions *out* of quiet are never delayed — see
/// [`PaneMachine::observe`].
pub const CONFIRM_TICKS: u32 = 3;

/// The kernel's view of a pane, from the tty's foreground process
/// group and the shell's own children.  Total: every pane maps to
/// exactly one variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Generic {
    /// The shell owns the tty and has no jobs at all.  Nothing of the
    /// user's is alive in this pane.
    Idle,
    /// The shell owns the tty, but jobs exist off the foreground —
    /// suspended (`^Z`) or backgrounded (`cmd &`).
    ///
    /// This variant is the reason the machine exists: the old model
    /// reported this pane as "at a prompt", i.e. indistinguishable
    /// from `Idle`, so a pane with a suspended claude in it looked
    /// safe to reclaim.
    PromptWithJobs { stopped: u16, running: u16 },
    /// A job owns the tty.
    Foreground { pgid: i32, leader: Option<JobLeader> },
    /// No information: the shell pid is gone, has no controlling
    /// terminal, or the tty reports no foreground group.
    Unknown,
}

/// A plugin's view of the program it understands, reported per pane.
/// Modelled on claudecode, which is the only reporter today; the
/// variants are deliberately about *turn ownership* rather than about
/// claude specifically, so a second plugin can use the same alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// No such program in this pane.  A real answer, not a gap.
    Absent,
    /// The program finished its turn and is waiting for the user.
    AwaitingUser,
    /// A tool call is outstanding — executing, or blocked on the user
    /// approving it.  Both are in flight.
    ToolPending { executing: bool },
    /// A turn is being produced.
    Working,
    /// The program was reclaimed while idle and the pane is holding a
    /// restore for it — nothing is running, but this is NOT an empty
    /// pane: acting on it (reclaiming the shell, closing the pane)
    /// silently discards a session that could still be resumed.
    ///
    /// It is a state rather than a plugin-private flag precisely so a
    /// second layer can see the debt.  The identity of what is parked
    /// (a session uuid, for claudecode) stays with the plugin; the
    /// machine only needs to know that something is owed.
    Dormant,
    /// Nobody has looked yet, or the transcript's shape wasn't
    /// recognised.  Never treat as idle.
    Unknown,
}

/// One observation of one pane: the machine's input symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub generic: Generic,
    pub activity: Activity,
}

/// Why a pane is busy.  Carried for the log and for any future UI; no
/// policy should branch on it — [`PaneMachine::quiescent`] is the
/// decision surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyKind {
    /// A plain job owns the tty (no reporting plugin bound).
    ForegroundJob,
    /// Jobs are suspended off the foreground.
    StoppedJobs,
    /// Jobs are running in the background.
    BackgroundJobs,
    /// The bound program is producing a turn.
    Working,
    /// The bound program has a tool call outstanding.
    ToolExecuting,
    /// …and is parked on the user's approval, which is the case that
    /// looks idle by every naive measure (silent tty, no children, old
    /// mtime) and is the most expensive one to get wrong.
    ToolAwaitingApproval,
}

/// The composed state of a pane.  Mutually exclusive and total over
/// `Generic × Activity` — see [`compose`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneStatus {
    /// Shell at its prompt, no jobs, no bound program: the pane is
    /// doing nothing at all.
    Empty,
    /// A program is bound and waiting for the user, with nothing in
    /// flight.
    AwaitingUser,
    /// Nothing is running, but a reclaimed program is parked here
    /// waiting to be restored.  Quiet, and carrying a debt: a policy
    /// that reclaims further (the shell, the pane itself) has to take
    /// the restore with it or drop it deliberately, not by accident.
    Dormant,
    /// Something is running or pending.
    Busy(BusyKind),
    /// The two views cannot both be true.  The pane is left alone and
    /// the disagreement is logged, because it means one of the two
    /// inputs is stale — usually a binding outliving its process.
    Contradiction { generic: &'static str, activity: &'static str },
    /// No information from at least one side.
    Unknown,
}

impl PaneStatus {
    /// Nothing of the user's work is in flight.  NOT the same as "may
    /// be acted on" — that additionally requires the machine to have
    /// held this state for [`CONFIRM_TICKS`], which is what
    /// [`PaneMachine::quiescent`] checks.
    pub fn is_quiet(&self) -> bool {
        matches!(
            self,
            PaneStatus::Empty | PaneStatus::AwaitingUser | PaneStatus::Dormant
        )
    }

    /// A restore is parked in this pane.  Anything that would take the
    /// pane further down (reclaim its shell, close it) must carry this
    /// forward or discard it on purpose.
    pub fn owes_restore(&self) -> bool {
        matches!(self, PaneStatus::Dormant)
    }

    /// Stable, greppable label for logs.
    pub fn label(&self) -> String {
        match self {
            PaneStatus::Empty => "empty".into(),
            PaneStatus::AwaitingUser => "awaiting_user".into(),
            PaneStatus::Dormant => "dormant".into(),
            PaneStatus::Busy(k) => format!("busy:{}", k.label()),
            PaneStatus::Contradiction { generic, activity } => {
                format!("contradiction:{}/{}", generic, activity)
            }
            PaneStatus::Unknown => "unknown".into(),
        }
    }
}

impl BusyKind {
    pub fn label(self) -> &'static str {
        match self {
            BusyKind::ForegroundJob => "fg_job",
            BusyKind::StoppedJobs => "stopped_jobs",
            BusyKind::BackgroundJobs => "bg_jobs",
            BusyKind::Working => "working",
            BusyKind::ToolExecuting => "tool_executing",
            BusyKind::ToolAwaitingApproval => "tool_awaiting_approval",
        }
    }
}

impl Generic {
    fn label(&self) -> &'static str {
        match self {
            Generic::Idle => "idle",
            Generic::PromptWithJobs { .. } => "prompt_with_jobs",
            Generic::Foreground { .. } => "foreground",
            Generic::Unknown => "unknown",
        }
    }
}

impl Activity {
    /// Stable, greppable label — used in logs and asserted by tests on
    /// both sides of the report channel.
    pub fn label(self) -> &'static str {
        match self {
            Activity::Absent => "absent",
            Activity::AwaitingUser => "awaiting_user",
            Activity::ToolPending { executing: true } => "tool_executing",
            Activity::ToolPending { executing: false } => "tool_awaiting_approval",
            Activity::Working => "working",
            Activity::Dormant => "dormant",
            Activity::Unknown => "unknown",
        }
    }
}

/// Fold one observation into a status.  The whole table, in one place:
///
/// | generic \ activity | Absent | AwaitingUser | Working / ToolPending | Unknown |
/// |---|---|---|---|---|
/// | `Idle`             | `Empty` | **contradiction** | **contradiction** | `Unknown` |
/// | `PromptWithJobs`   | `Busy(stopped/bg)` | `Busy(stopped/bg)` | `Busy(stopped/bg)` | `Busy(stopped/bg)` |
/// | `Foreground`       | `Busy(fg_job)` | `AwaitingUser` | `Busy(working/tool…)` | `Unknown` |
/// | `Unknown`          | `Unknown` | `Unknown` | `Unknown` | `Unknown` |
///
/// Two rows deserve their reasoning written down:
///
/// - `Idle` + a bound program is a **contradiction**, not a quiet
///   pane: the kernel says this pane has no processes while a plugin
///   claims a program lives in it, so one of the two is stale.
///   Rounding that to "idle" is exactly how a policy would end up
///   acting on a pane it does not understand.
/// - `PromptWithJobs` beats whatever the plugin says, because a
///   suspended program's transcript is frozen mid-whatever and cannot
///   describe the present.  Suspended is busy.
pub fn compose(generic: &Generic, activity: Activity) -> PaneStatus {
    match (generic, activity) {
        (Generic::Unknown, _) | (_, Activity::Unknown) => match generic {
            // A suspended/background job is a fact about the pane even
            // when the reporter has nothing to say.
            Generic::PromptWithJobs { stopped, running } => {
                PaneStatus::Busy(jobs_kind(*stopped, *running))
            }
            Generic::Foreground { .. } if activity == Activity::Unknown => {
                PaneStatus::Unknown
            }
            _ => PaneStatus::Unknown,
        },
        (Generic::Idle, Activity::Absent) => PaneStatus::Empty,
        // Reclaimed and parked: quiet like `Empty`, but distinguishable
        // from it, which is the whole point — an `Empty` pane may be
        // taken apart, a dormant one may not be taken apart *silently*.
        (Generic::Idle, Activity::Dormant) => PaneStatus::Dormant,
        (Generic::Idle, a) => PaneStatus::Contradiction {
            generic: generic.label(),
            activity: a.label(),
        },
        (Generic::PromptWithJobs { stopped, running }, _) => {
            PaneStatus::Busy(jobs_kind(*stopped, *running))
        }
        // A job owns the tty while a plugin still says "dormant": the
        // report is one scan behind (the program came back, or the user
        // started something).  The kernel is the fresher of the two, so
        // the pane reads busy; the plugin corrects itself next pass.
        (Generic::Foreground { .. }, Activity::Absent | Activity::Dormant) => {
            PaneStatus::Busy(BusyKind::ForegroundJob)
        }
        (Generic::Foreground { .. }, Activity::AwaitingUser) => PaneStatus::AwaitingUser,
        (Generic::Foreground { .. }, Activity::Working) => {
            PaneStatus::Busy(BusyKind::Working)
        }
        (Generic::Foreground { .. }, Activity::ToolPending { executing }) => {
            PaneStatus::Busy(if executing {
                BusyKind::ToolExecuting
            } else {
                BusyKind::ToolAwaitingApproval
            })
        }
    }
}

/// Suspended outranks backgrounded in the label: a `^Z`'d program is
/// the case a reader most needs to see.
fn jobs_kind(stopped: u16, _running: u16) -> BusyKind {
    if stopped > 0 {
        BusyKind::StoppedJobs
    } else {
        BusyKind::BackgroundJobs
    }
}

/// A committed change of state, for the caller to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub from: PaneStatus,
    pub to: PaneStatus,
    /// How long `from` had been held.
    pub held: Duration,
}

/// One pane's machine.
///
/// Time is passed in rather than read, so tests drive transitions
/// deterministically instead of sleeping.
pub struct PaneMachine {
    state: PaneStatus,
    entered_at: Instant,
    /// A quiet status seen but not yet committed, with how many
    /// consecutive observations have agreed on it.
    pending: Option<(PaneStatus, u32)>,
}

impl PaneMachine {
    pub fn new(now: Instant) -> Self {
        Self { state: PaneStatus::Unknown, entered_at: now, pending: None }
    }

    pub fn status(&self) -> &PaneStatus {
        &self.state
    }

    /// How long the current state has been held.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.entered_at)
    }

    /// The decision surface: nothing of the user's work is in flight,
    /// AND the machine has seen that consistently rather than in one
    /// lucky sample.  Everything else — including every flavour of
    /// "no information" — answers false.
    pub fn quiescent(&self) -> bool {
        self.state.is_quiet()
    }

    /// Feed one observation.  Returns `Some(Change)` when the state
    /// actually moved.
    ///
    /// Entering a quiet state waits for [`CONFIRM_TICKS`] agreeing
    /// observations; every other transition commits at once.  The
    /// asymmetry is the point: a pane that flickers through
    /// "awaiting user" between two tool calls must not become
    /// actionable, while a pane that starts doing something must stop
    /// being actionable immediately.
    pub fn observe(&mut self, obs: Observation, now: Instant) -> Option<Change> {
        let next = compose(&obs.generic, obs.activity);
        if next == self.state {
            // Steady state.  Any half-built candidate is stale.
            self.pending = None;
            return None;
        }
        if next.is_quiet() {
            let count = match self.pending.take() {
                Some((cand, n)) if cand == next => n + 1,
                _ => 1,
            };
            if count < CONFIRM_TICKS {
                self.pending = Some((next, count));
                return None;
            }
        }
        self.pending = None;
        Some(self.commit(next, now))
    }

    fn commit(&mut self, next: PaneStatus, now: Instant) -> Change {
        let change = Change {
            from: std::mem::replace(&mut self.state, next.clone()),
            to: next,
            held: now.saturating_duration_since(self.entered_at),
        };
        self.entered_at = now;
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn fg() -> Generic {
        Generic::Foreground { pgid: 600, leader: None }
    }

    fn obs(generic: Generic, activity: Activity) -> Observation {
        Observation { generic, activity }
    }

    /// Drive `n` identical observations, returning the changes.
    fn feed(m: &mut PaneMachine, o: Observation, n: u32, base: Instant) -> Vec<Change> {
        (0..n)
            .filter_map(|i| m.observe(o.clone(), base + Duration::from_secs(i as u64 + 1)))
            .collect()
    }

    // ── the composition table ─────────────────────────────────────

    #[test]
    fn compose_is_total_over_the_alphabets() {
        let generics = [
            Generic::Idle,
            Generic::PromptWithJobs { stopped: 1, running: 0 },
            Generic::PromptWithJobs { stopped: 0, running: 2 },
            fg(),
            Generic::Unknown,
        ];
        let activities = [
            Activity::Absent,
            Activity::AwaitingUser,
            Activity::Working,
            Activity::ToolPending { executing: true },
            Activity::ToolPending { executing: false },
            Activity::Dormant,
            Activity::Unknown,
        ];
        // Every pair maps somewhere, and only the two intended pairs
        // are quiet.  This is the property the whole design rests on,
        // so it is asserted over the full cross product rather than
        // spot-checked.
        let mut quiet = Vec::new();
        for g in &generics {
            for a in activities {
                let s = compose(g, a);
                if s.is_quiet() {
                    quiet.push((g.label(), a.label(), s));
                }
            }
        }
        assert_eq!(
            quiet,
            vec![
                ("idle", "absent", PaneStatus::Empty),
                ("idle", "dormant", PaneStatus::Dormant),
                ("foreground", "awaiting_user", PaneStatus::AwaitingUser),
            ],
            "exactly three pairs may be quiet, and each means something \
             different: nothing here, something parked here, something \
             waiting for you"
        );
        // Only one of the quiet states carries a debt.
        assert!(PaneStatus::Dormant.owes_restore());
        assert!(!PaneStatus::Empty.owes_restore());
        assert!(!PaneStatus::AwaitingUser.owes_restore());
    }

    /// The regression the machine was built for: a suspended job is
    /// busy, not idle.  The old model reported this pane as "at a
    /// prompt", indistinguishable from an empty one.
    #[test]
    fn a_suspended_job_is_busy_not_empty() {
        let s = compose(
            &Generic::PromptWithJobs { stopped: 1, running: 0 },
            Activity::AwaitingUser,
        );
        assert_eq!(s, PaneStatus::Busy(BusyKind::StoppedJobs));
        assert!(!s.is_quiet(), "a ^Z'd program must never read as quiet");
    }

    #[test]
    fn a_background_job_is_busy_too() {
        let s = compose(
            &Generic::PromptWithJobs { stopped: 0, running: 1 },
            Activity::Absent,
        );
        assert_eq!(s, PaneStatus::Busy(BusyKind::BackgroundJobs));
    }

    /// A binding that outlives its process: the kernel says the pane
    /// is empty while the transcript says a turn is in flight.
    #[test]
    fn a_stale_binding_is_a_contradiction_not_a_quiet_pane() {
        for a in [
            Activity::Working,
            Activity::AwaitingUser,
            Activity::ToolPending { executing: false },
        ] {
            let s = compose(&Generic::Idle, a);
            assert!(
                matches!(s, PaneStatus::Contradiction { .. }),
                "idle + {a:?} must be a contradiction, got {s:?}"
            );
            assert!(!s.is_quiet());
        }
    }

    #[test]
    fn unknown_on_either_side_poisons_unless_jobs_are_known() {
        assert_eq!(
            compose(&Generic::Unknown, Activity::AwaitingUser),
            PaneStatus::Unknown
        );
        assert_eq!(compose(&fg(), Activity::Unknown), PaneStatus::Unknown);
        // …but a suspended job is a fact even with no reporter.
        assert_eq!(
            compose(
                &Generic::PromptWithJobs { stopped: 2, running: 0 },
                Activity::Unknown
            ),
            PaneStatus::Busy(BusyKind::StoppedJobs)
        );
    }

    /// A dormant pane is quiet but is NOT an empty one.  A second
    /// layer reclaiming shells must be able to tell them apart, which
    /// is exactly what the plugin-private flag it replaces could not
    /// offer.
    #[test]
    fn a_dormant_pane_is_quiet_but_distinguishable_from_empty() {
        let dormant = compose(&Generic::Idle, Activity::Dormant);
        assert_eq!(dormant, PaneStatus::Dormant);
        assert!(dormant.is_quiet());
        assert!(dormant.owes_restore());
        assert_ne!(dormant, compose(&Generic::Idle, Activity::Absent));
    }

    /// The program came back (woken, or restarted by hand) before the
    /// plugin's next report: the kernel wins, the pane reads busy.
    #[test]
    fn a_stale_dormant_report_loses_to_a_running_job() {
        assert_eq!(
            compose(&fg(), Activity::Dormant),
            PaneStatus::Busy(BusyKind::ForegroundJob)
        );
    }

    #[test]
    fn an_unbound_pane_running_something_is_a_plain_job() {
        assert_eq!(
            compose(&fg(), Activity::Absent),
            PaneStatus::Busy(BusyKind::ForegroundJob)
        );
    }

    // ── the machine ───────────────────────────────────────────────

    #[test]
    fn entering_a_quiet_state_needs_confirmation() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        let quiet = obs(fg(), Activity::AwaitingUser);
        for i in 1..CONFIRM_TICKS {
            assert!(
                m.observe(quiet.clone(), base + Duration::from_secs(i as u64)).is_none(),
                "commit at observation {i} — too early"
            );
            assert!(!m.quiescent());
        }
        let change = m
            .observe(quiet, base + Duration::from_secs(CONFIRM_TICKS as u64))
            .expect("commit on the CONFIRM_TICKS-th agreeing observation");
        assert_eq!(change.to, PaneStatus::AwaitingUser);
        assert!(m.quiescent());
    }

    #[test]
    fn leaving_a_quiet_state_is_immediate() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        feed(&mut m, obs(fg(), Activity::AwaitingUser), CONFIRM_TICKS, base);
        assert!(m.quiescent());
        let change = m
            .observe(obs(fg(), Activity::Working), base + Duration::from_secs(10))
            .expect("busy commits at once");
        assert_eq!(change.to, PaneStatus::Busy(BusyKind::Working));
        assert!(!m.quiescent());
    }

    /// A pane that flickers through "awaiting user" between two tool
    /// calls must never become actionable.
    #[test]
    fn flapping_never_reaches_a_quiet_state() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        let quiet = obs(fg(), Activity::AwaitingUser);
        let busy = obs(fg(), Activity::ToolPending { executing: true });
        for i in 0..20 {
            let o = if i % 2 == 0 { quiet.clone() } else { busy.clone() };
            m.observe(o, base + Duration::from_secs(i + 1));
            assert!(
                !m.quiescent(),
                "alternating observations must not confirm a quiet state"
            );
        }
    }

    /// Confirmation counts CONSECUTIVE agreement: an interruption
    /// restarts it, it does not accumulate.
    #[test]
    fn confirmation_restarts_after_an_interruption() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        let quiet = obs(fg(), Activity::AwaitingUser);
        m.observe(quiet.clone(), base + Duration::from_secs(1));
        m.observe(quiet.clone(), base + Duration::from_secs(2));
        // One dissenting sample resets the count…
        m.observe(obs(fg(), Activity::Working), base + Duration::from_secs(3));
        m.observe(quiet.clone(), base + Duration::from_secs(4));
        m.observe(quiet.clone(), base + Duration::from_secs(5));
        assert!(!m.quiescent(), "two fresh agreements are not three");
        m.observe(quiet, base + Duration::from_secs(6));
        assert!(m.quiescent());
    }

    #[test]
    fn steady_state_emits_no_changes_and_keeps_its_clock() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        feed(&mut m, obs(fg(), Activity::Working), 1, base);
        let changes = feed(&mut m, obs(fg(), Activity::Working), 5, base);
        assert!(changes.is_empty(), "unchanged observations are silent");
        assert!(
            m.age(base + Duration::from_secs(30)) >= Duration::from_secs(29),
            "the clock measures the state, not the sweep"
        );
    }

    #[test]
    fn a_change_reports_how_long_the_previous_state_held() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        m.observe(obs(fg(), Activity::Working), base + Duration::from_secs(1));
        let c = m
            .observe(
                obs(fg(), Activity::ToolPending { executing: true }),
                base + Duration::from_secs(61),
            )
            .expect("state moved");
        assert_eq!(c.from, PaneStatus::Busy(BusyKind::Working));
        assert_eq!(c.held, Duration::from_secs(60));
    }

    /// Contradictions commit immediately (they are not quiet) so the
    /// log records the disagreement the moment it appears.
    #[test]
    fn a_contradiction_commits_at_once_and_is_never_quiescent() {
        let base = t0();
        let mut m = PaneMachine::new(base);
        let c = m
            .observe(
                obs(Generic::Idle, Activity::Working),
                base + Duration::from_secs(1),
            )
            .expect("contradiction commits at once");
        assert!(matches!(c.to, PaneStatus::Contradiction { .. }));
        assert!(!m.quiescent());
    }

    #[test]
    fn labels_are_stable_for_grepping() {
        assert_eq!(PaneStatus::Empty.label(), "empty");
        assert_eq!(PaneStatus::AwaitingUser.label(), "awaiting_user");
        assert_eq!(
            PaneStatus::Busy(BusyKind::StoppedJobs).label(),
            "busy:stopped_jobs"
        );
        assert_eq!(
            PaneStatus::Contradiction { generic: "idle", activity: "working" }.label(),
            "contradiction:idle/working"
        );
        assert_eq!(PaneStatus::Unknown.label(), "unknown");
    }
}
