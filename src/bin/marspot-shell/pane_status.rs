//! Per-pane state machines, swept in L1.
//!
//! The machine itself is `marspot::pane_state` (states, transition
//! rules, hysteresis — all pure and unit-tested); this module owns one
//! instance per session and feeds it.  Splitting them that way is what
//! makes the transition table testable without processes, ttys, or a
//! running shell.
//!
//! Each sweep builds one [`Observation`] per pane from two sources:
//!
//! - the **kernel half** via `pidtree::observe_pane` — who owns the
//!   tty, and what jobs the shell is still holding (suspended or
//!   backgrounded).  No shell cooperation, 2 syscalls for a quiet pane.
//! - the **plugin half** — whatever a plugin last reported for that
//!   session (claudecode reports what its transcript says).  A session
//!   with no report is [`Activity::Absent`] once some plugin has
//!   reported at least once, and [`Activity::Unknown`] before that:
//!   "no claude here" and "nobody has looked yet" are different facts.
//!
//! Why L1 owns this: the consumers are plugins and (later) policies
//! that live here, the session registry is readable here, and nothing
//! has to cross a wire.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use marspot::pane_state::{Activity, Change, Generic, Observation, PaneMachine, PaneStatus};
use marspot::pidtree;

/// How often the machines are stepped.  Rides `poll_supervisor`'s
/// ~250 ms cadence but gates itself to this, so the syscall cost is
/// ~2-4 per pane per second — the same order as L2's cwd sweep.
///
/// This is also the machine's clock: `CONFIRM_TICKS` agreeing
/// observations at this interval is how long a pane must look quiet
/// before anything may act on it.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// How long a bound program must sit finished before its pane starts
/// to recede.
///
/// Short relative to the reclamation threshold on purpose: the dim is
/// the warning, and a warning that appears at the same moment as the
/// action is not one.  A pane at level 1 is saying "this session is
/// drifting out"; a pane at level 2 has already gone.
pub const RESTING_AFTER: Duration = Duration::from_secs(300);

/// How long a pane's terminal must stay silent before the pane counts
/// as quiet.
///
/// Generous next to what it is watching for — a spinner writes several
/// times a second — but the point is to survive a program that pauses
/// between frames, not to react fast.  Nothing downstream cares about
/// the difference between 15 and 30 seconds; the thresholds that act
/// are minutes away.
pub const PTY_QUIET_AFTER: Duration = Duration::from_secs(30);

/// Recede level for a pane, from its machine state.
///
/// Deliberately **not** a stopwatch over every quiet pane.  A shell
/// sitting at its prompt is a terminal waiting for its user, however
/// long it waits — dimming those marked 15 of 18 panes here, which is
/// the same as marking none.  What earns a level is a *session*
/// receding: a bound program that finished its turn (1), and one that
/// has since been reclaimed (2).
pub fn recede_level_for(status: &PaneStatus, held: Duration) -> u32 {
    match status {
        PaneStatus::Dormant => 2,
        PaneStatus::AwaitingUser if held >= RESTING_AFTER => 1,
        _ => 0,
    }
}

/// A committed transition, tagged with the session it belongs to.
#[derive(Debug, Clone)]
pub struct SessionChange {
    pub sid: u64,
    pub change: Change,
}

/// Longest a persisted clock is believed.  A pane genuinely idle for
/// a month is indistinguishable from a state file left behind by
/// something that went wrong, and the wrong answer in that direction
/// reclaims a session on evidence nobody checked.
const MAX_PERSISTED_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// `shelld_session_id → (status label, when it started)`, as written
/// to disk.  Labels rather than the enum: this file outlives the build
/// that wrote it, and a label that no longer exists simply fails to
/// match, which is the correct outcome.
type ClockFile = HashMap<u64, (String, SystemTime)>;

/// `sid → bytelog size when the clocks were last written`.  Restored
/// alongside them; see [`PaneStateTracker::pty_quiet`].
type ByteFile = HashMap<u64, u64>;

/// One machine per live session, plus the sweep clock.
///
/// Bounded by construction: every sweep drops machines for sessions
/// the registry no longer lists, so a closed pane is forgotten on the
/// next tick rather than accumulating for the life of the shell
/// (CLAUDE.md §3).
pub struct PaneStateTracker {
    machines: HashMap<u64, PaneMachine>,
    /// `sid → (bytelog size, when it last changed)`.  The pane's own
    /// output, which is the bluntest "is anything happening" signal
    /// there is.
    pty_bytes: HashMap<u64, (u64, Instant)>,
    /// Clocks an earlier process left behind, consumed as each pane's
    /// state is re-confirmed.  Entries are removed once used or once
    /// contradicted, so a stale one cannot be applied twice.
    restorable: ClockFile,
    /// Bytelog sizes from the same file, consumed the first time each
    /// pane is looked at.  Without these the clocks above are useless:
    /// a fresh process has no idea when each pane last spoke, calls
    /// them all busy for 30 s, and the restored clock — written for a
    /// quiet state — fails to match and is thrown away.
    restorable_bytes: ByteFile,
    /// What each plugin last reported per session.  Absent key = no
    /// plugin has spoken for that pane.
    reported: HashMap<u64, Activity>,
    /// True once any plugin has reported anything at all.  Before
    /// that, a pane with no report is `Unknown` rather than `Absent` —
    /// claiming "there is no claude here" while the scanner is still
    /// warming up would be a lie with the same shape as the truth.
    any_report_seen: bool,
    last_sweep: Option<Instant>,
}

impl PaneStateTracker {
    pub fn new() -> Self {
        Self {
            machines: HashMap::new(),
            pty_bytes: HashMap::new(),
            restorable: HashMap::new(),
            restorable_bytes: HashMap::new(),
            reported: HashMap::new(),
            any_report_seen: false,
            last_sweep: None,
        }
    }

    /// Has this pane's PTY been silent long enough to call it quiet?
    ///
    /// Read from the session's own bytelog: L3 appends every byte the
    /// PTY produces, so its size is a running total of "how much has
    /// this pane said".  One `stat` per pane per sweep.
    ///
    /// This is the signal the transcript and the process table both
    /// miss.  Measured on this desktop over twelve seconds: a claude
    /// mid-thought wrote 695 bytes (its spinner), two working sessions
    /// wrote ~10 KB each, and a claude genuinely waiting for its user
    /// wrote **zero** — while all four looked identical to every other
    /// check, because a finished turn plus a long think leaves no new
    /// record, no child process and no measurable CPU.
    fn pty_quiet(&mut self, sid: u64, now: Instant) -> bool {
        let path = marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog");
        // No bytelog at all — a pane that has never had a PTY behind
        // it, or a test harness — says nothing, and "says nothing" is
        // quiet.  Reading a missing file as "just spoke" would freeze
        // every such pane at busy forever.
        let Ok(size) = std::fs::metadata(&path).map(|m| m.len()) else {
            self.pty_bytes.remove(&sid);
            return true;
        };
        let seed = match self.restorable_bytes.remove(&sid) {
            // Same size the previous process recorded: this pane has
            // not said a word since, however long ago that was.  Seed
            // it as already quiet — otherwise every silent update
            // declares all eighteen panes busy for half a minute, which
            // (being a different state) discards the very clocks the
            // restart just restored, and a threshold measured in
            // minutes can never be reached by a terminal that updates
            // itself.
            Some(prev) if prev == size => now.checked_sub(PTY_QUIET_AFTER).unwrap_or(now),
            _ => now,
        };
        let entry = self.pty_bytes.entry(sid).or_insert((size, seed));
        if entry.0 != size {
            *entry = (size, now);
        }
        now.duration_since(entry.1) >= PTY_QUIET_AFTER
    }

    /// Where the clocks live between processes.  Inside the state dir,
    /// so a sandbox run cannot read or clobber the installed app's.
    fn clock_path() -> std::path::PathBuf {
        marspot_term::paths::state_root().join("pane-state-clock.tsv")
    }

    /// Read what an earlier process left.  Best-effort: a missing or
    /// unreadable file just means every pane's clock starts now, which
    /// is the old behaviour and never wrong in the dangerous direction.
    pub fn load_clocks(&mut self) -> usize {
        let Ok(text) = std::fs::read_to_string(Self::clock_path()) else {
            return 0;
        };
        for line in text.lines() {
            let mut it = line.split('\t');
            let Some(sid) = it.next().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            let Some(label) = it.next().filter(|l| !l.is_empty()) else {
                continue;
            };
            let Some(secs) = it.next().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            self.restorable
                .insert(sid, (label.to_string(), UNIX_EPOCH + Duration::from_secs(secs)));
            // Fourth column arrived after the third; a file written by
            // an older build simply has no byte counts, and those panes
            // start their quiet timer now.
            if let Some(bytes) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                self.restorable_bytes.insert(sid, bytes);
            }
        }
        self.restorable.len()
    }

    /// Persist the current clocks.  Called after a sweep that changed
    /// something — one line per live pane, so the file is bounded by
    /// the pane count and rewritten whole.
    fn save_clocks(&self, now: Instant) {
        let mut out = String::new();
        for (sid, m) in &self.machines {
            // `Instant` has no epoch; convert through the age, which is
            // what actually has to survive.
            let started = SystemTime::now() - m.age(now);
            let secs = started.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let bytes = self.pty_bytes.get(sid).map(|(n, _)| *n).unwrap_or(0);
            out.push_str(&format!("{}\t{}\t{}\t{}\n", sid, m.status().label(), secs, bytes));
        }
        let path = Self::clock_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tsv.tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Record a plugin's view of one session.  Called from the plugin
    /// channel drain; the value is consumed by the next sweep.
    pub fn report_activity(&mut self, sid: u64, activity: Activity) {
        self.any_report_seen = true;
        self.reported.insert(sid, activity);
    }

    /// Current composed status + how long it has held, for every live
    /// session — what `PluginHost::pane_status` serves.
    pub fn snapshot(&self, now: Instant) -> HashMap<u64, (PaneStatus, Duration, bool)> {
        self.machines
            .iter()
            .map(|(sid, m)| (*sid, (m.status().clone(), m.age(now), m.quiescent())))
            .collect()
    }

    /// Let the next [`sweep`](Self::sweep) run whatever the interval
    /// gate would have said.
    ///
    /// For the moments where waiting up to `SWEEP_INTERVAL` is
    /// visible: a pane whose picture was frozen has been wearing a
    /// frozen brightness with it, and both have to come back in the
    /// same beat as the program that was restored into it.
    pub fn force_next_sweep(&mut self) {
        self.last_sweep = None;
    }

    /// Step every live session's machine, at most once per
    /// [`SWEEP_INTERVAL`].
    ///
    /// `None` = the interval gate skipped this call; `Some(vec)` = the
    /// machines were stepped and these are the committed transitions
    /// (possibly empty).  The caller needs the distinction: a pane
    /// *closing* removes a machine without producing a transition, so
    /// treating an empty vec as "nothing to do" would leave the closed
    /// pane's last status readable forever.
    pub fn sweep(&mut self, now: Instant) -> Option<Vec<SessionChange>> {
        if self.last_sweep.is_some_and(|t| now.duration_since(t) < SWEEP_INTERVAL) {
            return None;
        }
        self.last_sweep = Some(now);
        let panes: Vec<(u64, i32)> = marspot_term::session_registry::list_session_entries()
            .into_iter()
            .map(|e| (e.id, e.shell_child_pid))
            .collect();
        let changes = self.sweep_with(panes, now, pidtree::observe_pane);
        // Write only when something moved: in steady state this is a
        // sweep that touches no disk at all, which is what lets it run
        // once a second forever.
        if !changes.is_empty() {
            self.save_clocks(now);
        }
        Some(changes)
    }

    /// Sweep body with the pane list and the kernel probe injected —
    /// the bookkeeping (which machines exist, what each is fed, what
    /// is forgotten) is what's worth testing, and it shouldn't need
    /// real panes on a real tty to test.
    pub fn sweep_with<F>(
        &mut self,
        panes: Vec<(u64, i32)>,
        now: Instant,
        mut probe: F,
    ) -> Vec<SessionChange>
    where
        F: FnMut(i32) -> Generic,
    {
        let mut changes = Vec::new();
        let live: Vec<u64> = panes.iter().map(|(sid, _)| *sid).collect();
        for (sid, shell_pid) in panes {
            let activity = match self.reported.get(&sid) {
                Some(a) => *a,
                None if self.any_report_seen => Activity::Absent,
                None => Activity::Unknown,
            };
            let obs = Observation {
                generic: probe(shell_pid),
                activity,
                pty_quiet: self.pty_quiet(sid, now),
            };
            let machine = self
                .machines
                .entry(sid)
                .or_insert_with(|| PaneMachine::new(now));
            if let Some(change) = machine.observe(obs, now) {
                // A machine that has just re-confirmed the state an
                // earlier process left behind inherits that state's
                // clock: the pane did not become idle when this process
                // started watching it.  Consumed once — a restored
                // clock that no longer matches is dropped, not kept
                // around for a later state to pick up by accident.
                if let Some((label, since)) = self.restorable.remove(&sid) {
                    if label == change.to.label() {
                        let age = SystemTime::now()
                            .duration_since(since)
                            .unwrap_or_default();
                        if age <= MAX_PERSISTED_AGE {
                            machine.backdate(now - age, now);
                        }
                    }
                }
                changes.push(SessionChange { sid, change });
            }
        }
        // Sessions that vanished from the registry are gone; their
        // machines and their last reports go with them.
        self.machines.retain(|sid, _| live.contains(sid));
        self.reported.retain(|sid, _| live.contains(sid));
        self.pty_bytes.retain(|sid, _| live.contains(sid));
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot::pane_state::{BusyKind, CONFIRM_TICKS};

    fn fg() -> Generic {
        Generic::Foreground { pgid: 600, leader: None }
    }

    /// `MARSPOT_STATE_DIR` is process-global, so the tests that touch
    /// real files serialise on this.  (nextest is process-per-test and
    /// immune; keep `cargo test` correct too.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Step the tracker `n` times, one `SWEEP_INTERVAL` apart.
    fn steps<F: FnMut(i32) -> Generic + Copy>(
        t: &mut PaneStateTracker,
        panes: &[(u64, i32)],
        base: Instant,
        n: u64,
        probe: F,
    ) -> Vec<SessionChange> {
        (1..=n)
            .flat_map(|i| t.sweep_with(panes.to_vec(), base + SWEEP_INTERVAL * i as u32, probe))
            .collect()
    }

    #[test]
    fn a_pane_with_no_plugin_report_yet_is_unknown_not_absent() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        let out = steps(&mut t, &[(1, 100)], base, 5, |_| Generic::Idle);
        // Unknown poisons, so no quiet state can be reached and the
        // only commit is the one INTO Unknown (from the initial
        // Unknown → nothing, since they are equal).
        assert!(out.is_empty(), "unknown → unknown is not a transition");
        let snap = t.snapshot(base + Duration::from_secs(10));
        assert_eq!(snap[&1].0, PaneStatus::Unknown);
        assert!(!snap[&1].2, "never quiescent without information");
    }

    #[test]
    fn an_absent_report_plus_an_idle_shell_becomes_empty_after_confirmation() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        // Some other pane reported, so "no report" now means absent.
        t.report_activity(99, Activity::Working);
        let out = steps(
            &mut t,
            &[(1, 100)],
            base,
            CONFIRM_TICKS as u64,
            |_| Generic::Idle,
        );
        assert_eq!(out.len(), 1, "one commit, after confirmation");
        assert_eq!(out[0].change.to, PaneStatus::Empty);
        assert!(t.snapshot(base + SWEEP_INTERVAL * 4)[&1].2);
    }

    /// The case the machine exists for, end to end through the
    /// tracker: a suspended job keeps the pane out of every quiet
    /// state no matter what the plugin says.
    #[test]
    fn a_suspended_job_keeps_the_pane_busy_through_the_tracker() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::AwaitingUser);
        let out = steps(&mut t, &[(1, 100)], base, 6, |_| {
            Generic::PromptWithJobs { stopped: 1, running: 0 }
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].change.to, PaneStatus::Busy(BusyKind::StoppedJobs));
        assert!(!t.snapshot(base + SWEEP_INTERVAL * 7)[&1].2);
    }

    #[test]
    fn a_closed_pane_is_forgotten_so_the_maps_stay_bounded() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::Working);
        t.report_activity(2, Activity::Working);
        t.sweep_with(vec![(1, 100), (2, 200)], base + SWEEP_INTERVAL, |_| fg());
        assert_eq!(t.snapshot(base).len(), 2);
        t.sweep_with(vec![(2, 200)], base + SWEEP_INTERVAL * 2, |_| fg());
        let snap = t.snapshot(base);
        assert_eq!(snap.len(), 1, "pane 1 closed; its machine must not linger");
        assert!(snap.contains_key(&2));
    }

    /// Idle is a property of the pane, not of the process watching it:
    /// a pane that was already quiet keeps its clock across a restart,
    /// so an hour-scale threshold survives a silent update.
    #[test]
    fn a_re_confirmed_state_inherits_the_clock_a_previous_process_left() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::AwaitingUser);
        t.restorable.insert(
            1,
            ("awaiting_user".into(), SystemTime::now() - Duration::from_secs(7200)),
        );
        steps(&mut t, &[(1, 100)], base, CONFIRM_TICKS as u64, |_| fg());
        let (status, held, quiescent) = t.snapshot(base + SWEEP_INTERVAL * 4)[&1].clone();
        assert_eq!(status, PaneStatus::AwaitingUser);
        assert!(quiescent);
        assert!(
            held >= Duration::from_secs(7200),
            "the clock should carry over, got {held:?}"
        );
    }

    /// A clock only applies to the state it was written for.  A pane
    /// that came back in a *different* state starts fresh — otherwise
    /// a restart could hand a busy pane hours of fake idleness.
    #[test]
    fn a_clock_for_a_different_state_is_discarded_not_reused() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::AwaitingUser);
        t.restorable
            .insert(1, ("empty".into(), SystemTime::now() - Duration::from_secs(7200)));
        steps(&mut t, &[(1, 100)], base, CONFIRM_TICKS as u64, |_| fg());
        let held = t.snapshot(base + SWEEP_INTERVAL * 4)[&1].1;
        assert!(
            held < Duration::from_secs(60),
            "a mismatched clock must not be applied, got {held:?}"
        );
        assert!(
            t.restorable.is_empty(),
            "and it must be consumed, not left for a later state to pick up"
        );
    }

    /// A clock older than the cap is not evidence of anything — a
    /// state file left behind by something that went wrong looks
    /// exactly like a pane idle for a month.
    #[test]
    fn an_ancient_clock_is_ignored() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::AwaitingUser);
        t.restorable.insert(
            1,
            (
                "awaiting_user".into(),
                SystemTime::now() - (MAX_PERSISTED_AGE + Duration::from_secs(60)),
            ),
        );
        steps(&mut t, &[(1, 100)], base, CONFIRM_TICKS as u64, |_| fg());
        let held = t.snapshot(base + SWEEP_INTERVAL * 4)[&1].1;
        assert!(held < Duration::from_secs(60), "got {held:?}");
    }

    /// The ladder tracks a session receding, not a stopwatch over
    /// quiet panes.
    #[test]
    fn recede_levels_follow_the_session_not_the_clock() {
        // A shell at its prompt is a terminal waiting for you, for as
        // long as it waits.
        assert_eq!(recede_level_for(&PaneStatus::Empty, Duration::from_secs(86_400)), 0);
        // A bound session that just finished a turn is still live…
        assert_eq!(recede_level_for(&PaneStatus::AwaitingUser, Duration::ZERO), 0);
        // …and starts to recede once it has been finished a while —
        // this is the pane that will be reclaimed if it keeps sitting.
        assert_eq!(recede_level_for(&PaneStatus::AwaitingUser, RESTING_AFTER), 1);
        // Reclaimed: gone until something brings it back.
        assert_eq!(recede_level_for(&PaneStatus::Dormant, Duration::ZERO), 2);
        // Anything working, or unreadable, stays at full strength.
        for s in [
            PaneStatus::Busy(BusyKind::Working),
            PaneStatus::Busy(BusyKind::ToolAwaitingApproval),
            PaneStatus::Busy(BusyKind::StoppedJobs),
            PaneStatus::Unknown,
            PaneStatus::Contradiction { generic: "idle", activity: "working" },
        ] {
            assert_eq!(recede_level_for(&s, Duration::from_secs(86_400)), 0, "{s:?}");
        }
    }

    /// The restored clock is worthless without the byte count that
    /// goes with it.
    ///
    /// A fresh process cannot know when each pane last spoke, so it
    /// starts every quiet timer at zero and calls all of them busy for
    /// thirty seconds.  Busy is a different state than the one the
    /// clock was written for, so the restore is discarded — and a
    /// threshold measured in minutes can never be reached by a
    /// terminal that silently updates itself.  Carrying the bytelog
    /// size across is what makes "this pane has said nothing since"
    /// answerable at startup.
    #[test]
    fn a_pane_that_has_said_nothing_since_the_restart_is_quiet_immediately() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "marspot-pane-clock-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let bytelog = dir.join("sessions").join("1").join("bytelog");
        std::fs::create_dir_all(bytelog.parent().unwrap()).expect("temp dir");
        std::fs::write(&bytelog, b"hello from the pty").expect("bytelog");
        // SAFETY: single-threaded within this test, guarded by ENV_LOCK.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };

        // One process watches the pane, sees it settle, and exits.
        let base = Instant::now();
        let mut first = PaneStateTracker::new();
        first.report_activity(1, Activity::AwaitingUser);
        // Long enough that its own quiet timer (30 s) has expired and
        // the pane has been settled for a good while after that.
        steps(&mut first, &[(1, 100)], base, 90, |_| fg());
        assert_eq!(
            first.snapshot(base + SWEEP_INTERVAL * 91)[&1].0,
            PaneStatus::AwaitingUser,
            "the first process should have settled before it saved"
        );
        first.save_clocks(base + SWEEP_INTERVAL * 91);

        // Its successor picks the pane up where it was left.
        let mut second = PaneStateTracker::new();
        assert_eq!(second.load_clocks(), 1);
        second.report_activity(1, Activity::AwaitingUser);
        let out = steps(&mut second, &[(1, 100)], base, CONFIRM_TICKS as u64, |_| fg());
        let (status, held, _) = second.snapshot(base + SWEEP_INTERVAL * 4)[&1].clone();
        assert_eq!(
            status,
            PaneStatus::AwaitingUser,
            "an unchanged bytelog means the pane never spoke, so it is quiet — \
             not busy for the first 30 s; got {out:?}"
        );
        assert!(
            held >= SWEEP_INTERVAL * 45,
            "and it keeps the clock its predecessor was holding, got {held:?}"
        );

        // A pane that DID speak while nobody was watching starts over.
        std::fs::write(&bytelog, b"hello from the pty, and then some more").expect("bytelog");
        let mut third = PaneStateTracker::new();
        third.load_clocks();
        third.report_activity(1, Activity::AwaitingUser);
        steps(&mut third, &[(1, 100)], base, CONFIRM_TICKS as u64, |_| fg());
        assert_eq!(
            third.snapshot(base + SWEEP_INTERVAL * 4)[&1].0,
            PaneStatus::Busy(BusyKind::Output),
            "new bytes since the save is exactly what the signal is for"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clock_file_from_an_older_build_still_loads() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "marspot-pane-clock-old-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // SAFETY: single-threaded within this test, guarded by ENV_LOCK.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        // Three columns — the shape before byte counts existed.
        std::fs::write(dir.join("pane-state-clock.tsv"), "7\tawaiting_user\t1700000000\n")
            .expect("clock file");

        let mut t = PaneStateTracker::new();
        assert_eq!(t.load_clocks(), 1, "the clock still parses");
        assert!(t.restorable_bytes.is_empty(), "with no byte count to go with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_interval_gate_skips_a_second_sweep_inside_the_window() {
        let mut t = PaneStateTracker::new();
        let base = Instant::now();
        assert!(t.sweep(base).is_some(), "first sweep runs");
        assert!(
            t.sweep(base + Duration::from_millis(250)).is_none(),
            "a second sweep inside the window must be a no-op"
        );
        assert!(t.sweep(base + SWEEP_INTERVAL).is_some());

        // …and the one caller that cannot wait out the window: a pane
        // whose picture has just been unfrozen needs its brightness
        // back in the same beat as its content, not up to a second
        // later.
        let just_after = base + SWEEP_INTERVAL + Duration::from_millis(10);
        assert!(t.sweep(just_after).is_none(), "still inside the window");
        t.force_next_sweep();
        assert!(t.sweep(just_after).is_some(), "forced through the gate");
        assert!(
            t.sweep(just_after).is_none(),
            "and the gate closes again behind it"
        );
    }

    /// A plugin's report is used by the NEXT sweep, and replaces the
    /// previous one — the tracker holds one view per session, not a
    /// history.
    #[test]
    fn the_latest_report_wins() {
        let base = Instant::now();
        let mut t = PaneStateTracker::new();
        t.report_activity(1, Activity::Working);
        t.report_activity(1, Activity::AwaitingUser);
        let out = steps(
            &mut t,
            &[(1, 100)],
            base,
            CONFIRM_TICKS as u64,
            |_| fg(),
        );
        assert_eq!(out.last().unwrap().change.to, PaneStatus::AwaitingUser);
    }
}
