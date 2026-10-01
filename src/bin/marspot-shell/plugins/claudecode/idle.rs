//! When a pane's claude is reclaimed, and what it takes to bring it
//! back.
//!
//! A session left sitting costs hundreds of megabytes; reclaiming one
//! the user is still reading costs them their place.  What this module
//! holds is the evidence side of that trade — how long quiet is quiet,
//! what counts as work in flight, what a parked pane remembers — and
//! the script that parks and restores.

use std::time::{Duration, SystemTime};

use super::*;

/// How long a pane must hold a quiet state before its claude is
/// reclaimed, and the knob to change or disable it.
///
/// Default half an hour.  The cost model has not changed — the prompt
/// cache's TTL is an hour, so reclaiming before it expires means the
/// next request re-sends the transcript that a warm cache would have
/// covered — but that cost is bounded and one-off, while 340 MB per
/// session is not, and a session woken by focus pays it anyway.
///
/// The user owns this one, so it comes from `settings.toml` and is
/// re-read on the pane sweep — changing it takes effect within the
/// second, with nothing restarted.
///
/// `MARSPOT_CC_IDLE_HIBERNATE_S` still overrides, and still wins: the
/// sandbox scripts and soak tests set it, and an env var is the right
/// shape for "this process, this run" against a file that means "what
/// the user wants, always".  `=0` turns it off entirely.
pub(super) fn hibernate_after() -> Option<Duration> {
    if let Some(secs) = std::env::var("MARSPOT_CC_IDLE_HIBERNATE_S")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return (secs > 0).then(|| Duration::from_secs(secs));
    }
    marspot::settings::get().reclaim_after()
}

/// How far back the CPU baseline is kept before being replaced.
///
/// The scan runs every ~2 s, and the first version replaced the
/// baseline on every pass — so the delta always spanned one scan and
/// the "sample must span at least 30 s" gate could never pass.  Live
/// proof: a pane sat past the one-hour threshold logging
/// `cpu sample spans only 2s`, i.e. reclamation would never have
/// fired at all.  Holding the baseline for a minute makes the delta
/// mean "cpu burned in the last minute", which is the question.
pub(super) const CPU_BASELINE_WINDOW: Duration = Duration::from_secs(60);

/// How long a fresh dormant record is immune to being judged by a scan.
///
/// SIGTERM is not instantaneous: claude flushes its transcript and
/// exits over several seconds, and every scan in that window still
/// finds it in the process table and still holds its binding.  Read
/// literally, that binding says "claude is back" — which drops the
/// record that was just written, and with it the pane's wake path.
///
/// Comfortably longer than the observed exit (a couple of ticks) and
/// far shorter than anything a person would notice.  The cost of being
/// too generous is a record that lingers a few seconds after a wake;
/// arming already asks the kernel whether the pane really has no
/// claude, so a lingering record cannot misfire.
pub(super) const KILL_GRACE: Duration = Duration::from_secs(15);

/// Processes a resting claude keeps around, measured on this host:
/// an MCP server per session (`smix-mcp`), a language server it
/// started (`rust-analyzer` and its proc-macro helper), and
/// `caffeinate` (short-lived, respawned).  None of them is work.
///
/// Anything else under claude IS work: a background task, a watcher,
/// a dev server, a `tail -f` a monitor is following.  Measured on a
/// live session: `zsh → cargo-fuzz + tail`, all at **0.0 % CPU** —
/// which is exactly why the CPU gate cannot be the only one.  Killing
/// claude kills that whole tree.
///
/// The list is a name allowlist, so it fails in the safe direction:
/// an unrecognised helper (a new MCP server, another language server)
/// reads as work and the session simply is not reclaimed.  The
/// opposite default would silently kill somebody's build.
pub(super) fn is_resting_helper(row: &pidtree::ProcRow, has_children: bool) -> bool {
    let name = row.comm.as_str();
    if name.contains("-mcp") || name.starts_with("mcp-") {
        return true;
    }
    if name.starts_with("rust-analyzer") || name.ends_with("-language-server") {
        return true;
    }
    if name == "caffeinate" {
        return true;
    }
    // claude keeps one shell around per session; an empty one is
    // furniture, one with something under it is a job in flight.
    if matches!(name, "zsh" | "bash" | "sh") && !has_children {
        return true;
    }
    false
}

/// Does this session have work of its own running?
///
/// Cheap: one pass over the descendants the scan already walked.
pub fn has_work_in_flight(claude_pid: i32, procs: &[pidtree::ProcRow]) -> bool {
    let descendants = pidtree::descendants_of(claude_pid, procs);
    descendants.iter().any(|d| {
        // A helper's own children (rust-analyzer's proc-macro server)
        // ride along with it; judge each row on its own name first.
        let has_children = procs.iter().any(|p| p.ppid == d.pid);
        !is_resting_helper(d, has_children)
            && !descendants.iter().any(|parent| {
                parent.pid == d.ppid
                    && is_resting_helper(
                        parent,
                        procs.iter().any(|p| p.ppid == parent.pid),
                    )
            })
    })
}

/// Is this session waiting on its own timer rather than on the user?
///
/// A `/loop` autorun schedules its next wake from inside claude —
/// nothing shows up in the process table, the transcript stops, and
/// the pane looks exactly like one waiting for a person.  Reclaiming
/// it kills the loop.  The one trace it leaves is the tool call, so
/// that is what this looks for in the tail window.
///
/// Heuristic and deliberately sticky: a stale hit means a session is
/// not reclaimed, which costs memory; a miss means someone's
/// autonomous run dies.
pub(super) fn tail_mentions_own_timer(text: &str) -> bool {
    text.contains("\"name\":\"ScheduleWakeup\"") || text.contains("<<autonomous-loop")
}

/// CPU a claude subtree may burn during the observation window and
/// still count as idle.  Not zero: an idling MCP server and a language
/// server both tick over.  50 ms across a window of many seconds is
/// well under 1 % of a core — an actual tool run is orders above it.
pub(super) const IDLE_CPU_TOLERANCE_NS: u64 = 50_000_000;

/// Everything the idle decision looks at, gathered so the rule itself
/// stays a pure function.
#[derive(Clone, Copy, Debug)]
pub(super) struct IdleEvidence {
    /// The state machine says nothing is in flight AND has held that
    /// view long enough to be believed (`CONFIRM_TICKS`).
    pub(super) quiescent: bool,
    /// The composed state is specifically "a program is bound and
    /// waiting for the user".  `Empty` is quiet too, but it means
    /// there is no claude here to reclaim.
    pub(super) awaiting_user: bool,
    /// How long that state has held.
    ///
    /// Kept for the log, no longer the clock that decides.  See
    /// `idle_for`.
    pub(super) held: Duration,
    /// How long since the session itself did anything — the age of its
    /// transcript.
    ///
    /// This is the clock that decides.  The pane's own quiet clock
    /// cannot be: claude writes `Checking for updates` into the corner
    /// every thirty minutes, which makes the terminal busy for half a
    /// minute and resets it.  Measured here: 23 of those in one pane's
    /// log, so a session idle for eleven hours never once accumulated
    /// thirty quiet minutes, and reclamation could not fire at all.
    pub(super) idle_for: Duration,
    /// CPU the claude subtree consumed since the previous sample, and
    /// how long ago that sample was taken.
    pub(super) cpu_delta_ns: u64,
    pub(super) since_sample: Duration,
    /// A process of the session's own is running — a background task,
    /// a watcher, a build.  CPU says nothing about these: measured on
    /// a live session, `zsh → cargo-fuzz + tail` sat at 0.0 %.
    pub(super) work_in_flight: bool,
    /// The session is waiting on a timer it set itself (a `/loop`
    /// autorun), not on the user.
    pub(super) own_timer: bool,
    /// The user's cursor is in this pane right now.
    ///
    /// The one thing the session's own clock cannot see.  A pane can
    /// be half an hour idle by its transcript while the user sits in
    /// it reading the last answer, and reclaiming it there charges
    /// them a three-second wake for their next keystroke — with no
    /// warning, because a reclamation is deliberately invisible.
    pub(super) user_here: bool,
}

/// Why a candidate pane was not reclaimed on this pass, as
/// `(category, line)`.
///
/// The category is what the caller de-duplicates on, and it is
/// deliberately value-free.  The first version returned one string
/// containing the current idle seconds — which changes every scan, so
/// "log when the reason changes" logged **every** scan: 3030 lines in
/// 25 minutes on this machine, about 1 MB/hour of noise that would
/// rotate the real history out of an 8 MB log in under a day.
///
/// Only computed for panes the machine already calls quiet and
/// awaiting their user — i.e. ones that are *going* to be reclaimed
/// once something finishes ticking.  Anything busier is not a
/// candidate and has nothing to explain.
pub(super) fn blocking_reason(
    e: IdleEvidence,
    threshold: Duration,
) -> Option<(&'static str, String)> {
    if should_hibernate(e, threshold) {
        return None;
    }
    if !(e.quiescent && e.awaiting_user) {
        return None; // not a candidate; not this log's business
    }
    if e.user_here {
        return Some((
            "user_here",
            "the user's cursor is in this pane".to_string(),
        ));
    }
    if e.work_in_flight {
        return Some((
            "work_in_flight",
            "a process of its own is running".to_string(),
        ));
    }
    if e.own_timer {
        return Some((
            "own_timer",
            "waiting on a timer it set itself".to_string(),
        ));
    }
    Some(if e.idle_for < threshold {
        (
            "below_threshold",
            format!(
                "idle {}s of {}s (terminal quiet {}s)",
                e.idle_for.as_secs(),
                threshold.as_secs(),
                e.held.as_secs()
            ),
        )
    } else if e.since_sample < Duration::from_secs(30) {
        (
            "short_cpu_sample",
            format!("cpu sample spans only {}s", e.since_sample.as_secs()),
        )
    } else {
        (
            "cpu_busy",
            format!(
                "subtree burned {}ms of cpu in {}s",
                e.cpu_delta_ns / 1_000_000,
                e.since_sample.as_secs()
            ),
        )
    })
}

/// May this pane's claude be reclaimed right now?
///
/// Deliberately conjunctive and default-deny: every unknown answers
/// false.  The expensive mistake is killing a session that was doing
/// something (an unanswered tool call, a background task, a turn in
/// flight); the cheap mistake is leaving memory on the table for
/// another hour.
pub(super) fn should_hibernate(e: IdleEvidence, threshold: Duration) -> bool {
    e.quiescent
        && e.awaiting_user
        // Never the seat the user is in.  Everything else here is
        // about the session; this is the one clause about the person.
        && !e.user_here
        // Two vetoes the clock cannot see: something of the session's
        // own is running, or the session is waiting on its own timer.
        // Both mean the pane is not resting, it is between steps.
        && !e.work_in_flight
        && !e.own_timer
        // The session's own clock, not the terminal's.  `held` resets
        // every time anything writes to the pane, and something does:
        // claude's thirty-minute update check.  With a thirty-minute
        // threshold that is a race the check always wins — measured
        // repeatedly at `held_s=1767`, thirty-three seconds short.
        && e.idle_for >= threshold
        // A CPU sample only means something once it spans real time;
        // the first sample after a restart spans none.  Half the
        // baseline window, so a pane becomes eligible partway through
        // one rather than having to wait for a full fresh one.
        && e.since_sample >= CPU_BASELINE_WINDOW / 2
        && e.cpu_delta_ns <= IDLE_CPU_TOLERANCE_NS
}

/// The pane's shell pid, straight from the registry.  0 when the
/// session is gone — the callers treat that as "cannot tell", which is
/// the honest reading.
pub(super) fn shell_pid_for(sid: u64) -> i32 {
    marspot_term::session_registry::list_session_entries()
        .into_iter()
        .find(|e| e.id == sid)
        .map(|e| e.shell_child_pid)
        .unwrap_or(0)
}

/// What this plugin reports for a pane it has no live binding in.
///
/// "No claude here" and "a claude was reclaimed from here and can be
/// restored" look identical from the outside — same absent process,
/// same shell at the same prompt — and they are not the same thing at
/// all.  Reporting them apart is what lets the state machine (and any
/// second layer built on it) see that a pane owes a restore, instead
/// of that fact living in a plugin-private set nobody else can read.
pub(super) fn activity_for_unbound(sid: u64, dormant: &[DormantRecord]) -> CcActivity {
    if dormant.iter().any(|d| d.shelld_sid == sid) {
        CcActivity::Dormant
    } else {
        CcActivity::Absent
    }
}

/// The chrome a held pane is frozen at.
///
/// A reclamation is supposed to be invisible: the picture stands still
/// at the frame the user left, and everything drawn *around* that
/// picture has to stand still with it, or the pane announces what is
/// happening to it even though its cells never moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FrozenLook {
    pub(super) badge: String,
    pub(super) title: String,
}

/// What a dormant pane needs to wake itself up again, persisted so it
/// survives an L1 self-execv (which drops every PaneSession).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DormantRecord {
    pub(super) shelld_sid: u64,
    pub(super) uuid: String,
    pub(super) profile_num: u8,
    /// The config dir observed on the live process, so a wake after a
    /// restart still resumes under the same account.
    pub(super) config_dir: Option<String>,
    /// When this pane was parked.  A record may only be judged
    /// ("is the program back?") by a scan that ran AFTER it was
    /// created — the scan that triggers the reclamation was taken
    /// while the program was still alive, so judging by it drops the
    /// record the instant it is made.  That shipped: `dormant.tsv`
    /// came out empty and the wake path would not have survived a
    /// restart.
    pub(super) created_at: SystemTime,
}



/// "We could not tell which profile this session belongs to."
///
/// Set when the badge reads `P?` (an unrecognised `CLAUDE_CONFIG_DIR`
/// shape) or when the env could not be read at all.  It is a refusal
/// marker, not a default: resuming under the wrong profile puts the
/// session in front of a different account.
pub(super) const PROFILE_UNKNOWN: u8 = u8::MAX;

/// Serialise the dormant set, one record per line.  Hand-rolled
/// because it is three fields and the project does not take a
/// serialisation dependency for that.
pub(super) fn encode_dormant(records: &[DormantRecord]) -> String {
    let mut s = String::new();
    for r in records {
        let secs = r
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            r.shelld_sid,
            r.uuid,
            r.profile_num,
            secs,
            r.config_dir.as_deref().unwrap_or("")
        ));
    }
    s
}

pub(super) fn decode_dormant(text: &str) -> Vec<DormantRecord> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split('\t');
            let sid = it.next()?.parse::<u64>().ok()?;
            let uuid = it.next()?.to_string();
            let profile = it.next()?.parse::<u8>().ok()?;
            // 4th column added later; a file without it decodes as
            // epoch, i.e. "old enough to be judged", which is the
            // right answer for a record from a previous process.
            let created_at = it
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| std::time::UNIX_EPOCH + Duration::from_secs(s))
                .unwrap_or(std::time::UNIX_EPOCH);
            let config_dir = it.next().map(|s| s.to_string());
            // A uuid is the only field that can be typo'd into
            // something dangerous (it lands in a shell command), so it
            // is checked here rather than at the write site.
            (!uuid.is_empty()
                && uuid.len() <= 64
                && uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .then_some(DormantRecord {
                shelld_sid: sid,
                uuid,
                profile_num: profile,
                // 5th column, added with the config-dir resume; an
                // older row without it falls back to the plain entry
                // point, which is the default profile.
                config_dir: config_dir.filter(|d| !d.is_empty() && pty_op::shell_safe(d)),
                created_at,
            })
        })
        .collect()
}

/// How long claude's output must stay still before its first frame
/// counts as finished.
///
/// Wall-clock, not ticks.  `on_tick` rides the redraw pump: ~250 ms
/// when the window is idle, but 16 ms while a pane is producing
/// frames — which is exactly the situation a wake is in.  Counting
/// three ticks meant 48 ms of silence, short enough to land in the
/// pause between claude's banner and its first paint, and the freeze
/// lifted there.  A duration is the same length whatever the cadence.
///
/// 2026-08-03 — raised from 500 ms.  claude does not paint a resumed
/// session in one go: it draws, pauses to read more of the transcript,
/// redraws, settles.  Half a second of silence is inside those pauses,
/// so the freeze lifted onto a half-built screen and the user watched
/// the rest of it arrive — the wake was over in 1.9 s on a session
/// that takes several to settle.  The point of the freeze is that the
/// only transition anyone sees is old frame → finished frame, so the
/// still-window has to be longer than claude's own pauses.  Waiting
/// too long costs nothing visible: what is on screen is the picture
/// the user left.  `WAKE_WATCHDOG` bounds the pathological case.
pub(super) const WAKE_QUIET_FOR: Duration = Duration::from_millis(1_500);

/// How long the hold gets to reach L3 before the signal is sent.
///
/// The request crosses two process boundaries (L1 → L2 → L3), each a
/// channel drained on its own loop; the signal crosses none.  Sub-
/// millisecond in practice, and the whole wait is invisible — the pane
/// is already showing the frame it will keep.
pub(super) const HOLD_SETTLE: Duration = Duration::from_millis(250);


/// Upper bound on holding the keyboard.  A resume that never draws
/// (claude missing, profile dir gone, PATH broken) must still give the
/// pane back rather than lock it forever.
pub(super) const WAKE_WATCHDOG: Duration = Duration::from_secs(30);






/// The reclamation script: hold the picture, take claude down, park
/// until the user comes back, then put it back the way it was.
///
/// This was a 200-line state machine.  What is left is the two things
/// that are actually claudecode's opinion — the command line, and what
/// counts as "claude is back" — with the sequencing, the deadlines, the
/// escalation and the cleanup owned by [`pty_op`](crate::plugins::pty_op).
///
/// `None` when the session's profile cannot be quoted: a session that
/// comes back under a *different account* is worse than one left
/// running, so there is no fallback line here.
pub(super) fn reclaim_op(
    uuid: &str,
    config_dir: Option<&str>,
    claude_pid: i32,
    shell_pid: i32,
) -> Option<pty_op::PtyOp> {
    // Same rule as `profile_cycle_op`: a session we cannot name cannot
    // be brought back, and taking claude down without a resume line
    // would lose it outright.
    if uuid.is_empty() {
        return None;
    }
    let mut cmd = pty_op::PtyCommand::new("claude").clear_screen_first(true);
    if let Some(dir) = config_dir {
        cmd = cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    // The pane we are about to bring back reports its own submits, so
    // the next thing typed into it is confirmed rather than guessed
    // at. Absent when marspot cannot name its own binary; the pane
    // then works exactly as it did before.
    if let Some(settings) = crate::receipts::claude_settings_arg() {
        cmd = cmd.arg("--settings").quoted_arg(settings);
    }
    let line = cmd.arg("--resume").arg(uuid).to_bytes()?;
    // Reclaiming parks claude and brings it back later. Anything the
    // person had typed and not sent lives in the process being parked,
    // so it is read now and handed back when the new one has painted.
    let held = pty_op::Job::new();
    Some(
        pty_op::PtyOp::new("cc.reclaim")
            .hold_screen(true)
            .step(pty_op::Step::capture_composer(std::sync::Arc::clone(&held)))
            // No Esc hatch: bailing out mid-park would leave a pane with
            // no claude and no wake armed, which is strictly worse than
            // waiting.  The wake itself takes ~2 s.
            .escape_hatch(false)
            // No badge of its own.  A reclamation the user can see is
            // a reclamation that happened *to* them; this one is
            // supposed to be indistinguishable from the pane sitting
            // there untouched, so the corner keeps saying what it said
            // before (`ClaudecodePlugin::held` re-asserts it).  Which
            // pane is parked is in the log and in `dormant.tsv`.
            // Let the hold reach L3 before anything can draw.  That
            // request crosses two process boundaries; the signal crosses
            // none, and sent together the signal wins — claude's parting
            // `Resume this session with: …` and the shell's prompt both
            // land on screen.
            .step(pty_op::Step::settle(HOLD_SETTLE).named("hold_settle"))
            .step(
                pty_op::Step::terminate(claude_pid, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(pty_op::Step::await_user().named(RECLAIM_PARK_LABEL))
            // Between parking and the user coming back, the session can
            // return by other means — a second wake armed on the same
            // pane, the user starting it themselves.  Typing then puts
            // `claude --resume …` into the running session's prompt,
            // where it sits as text they have to delete.  Seen on the
            // real machine, twice in one day.
            .step(pty_op::Step::stop_if_process(shell_pid, looks_like_claudecode))
            .step(pty_op::Step::send(line).named("resume"))
            .step(pty_op::Step::await_process(shell_pid, looks_like_claudecode))
            .step(
                pty_op::Step::await_quiet(WAKE_QUIET_FOR)
                    .after_bytes(FIRST_FRAME_BYTES)
                    .timeout(WAKE_WATCHDOG),
            )
            // Their sentence back where they left it.
            .step(pty_op::Step::restore_composer(held)),
    )
}

/// How much output counts as "it has drawn its first frame".
///
/// The process exists within milliseconds of the resume line — a fork
/// and an exec — but claude then reads the whole transcript before it
/// paints, and that pause is seconds long.  During it the terminal has
/// seen only the screen clear and a handful of mode sets, so "output
/// happened and then stopped" is true while the screen is *blank*.
/// Lifting the freeze there is the black flash the user kept seeing.
///
/// A real first frame is tens of kilobytes of text and colour; startup
/// noise is a few hundred bytes.  Two kilobytes sits between them with
/// room on both sides.
pub(super) const FIRST_FRAME_BYTES: u64 = 2048;

/// Where a re-armed run picks up: an L1 restart replaces this process
/// while the pane stays parked, so the new run must not kill anything
/// again — it starts at the step that waits for the user.
///
/// A label, not a position. This was `= 2`, and adding a step in
/// front of it moved the step it named without changing the number:
/// a re-arm would have resumed in the middle of the teardown.
pub(super) const RECLAIM_PARK_LABEL: &str = "reclaim_park";

#[cfg(test)]
mod park_label_tests {
    use super::*;

    /// The park step is findable by the name the re-arm looks for.
    ///
    /// The lookup falls back to step 0 when the name is missing, which
    /// would send a re-armed run through the teardown again instead of
    /// parking it -- killing a claude that is already gone and typing
    /// a resume nobody asked for. A name that does not resolve is
    /// worse than the number it replaced, so it is checked here rather
    /// than trusted.
    #[test]
    fn the_park_step_answers_to_its_name() {
        let op = reclaim_op("aaaa-bbbb", None, 0, 1234).expect("a script");
        let at = op.index_of(RECLAIM_PARK_LABEL).expect("the park step is named");
        assert!(at > 0, "the park is not the first step; it follows the teardown");
        assert_eq!(op.index_of("no-such-step"), None, "and the lookup can fail");
    }
}
