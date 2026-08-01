//! Keeping a rotation going without a person in the loop.
//!
//! A long-running session in a pane works in rotations: it does a
//! chunk, says it is done and that the context can be cleared, and then
//! waits.  Left alone it waits forever.  This is the policy that
//! notices and types the two lines a person would have typed — and,
//! separately, the one that nudges a session that has stalled on a
//! server error rather than on anything it did.
//!
//! Everything here is a *decision*: `decide` looks at what the pane
//! shows and returns what to type, and nothing else.  The looking and
//! the typing live in the supervisor, so this file can be tested
//! against a clock it owns, without a pane, a PTY or a program.
//!
//! The rules that matter, in the order they were paid for:
//!
//! 1. **Never type into a pane that is doing something.**  Every
//!    action requires the pane to be quiet *and* to have nothing
//!    running underneath it.  A line typed into a working session is
//!    at best ignored and at worst answered.
//! 2. **One action per quiet period.**  After acting, the policy waits
//!    for the pane to become busy again before it will act on the same
//!    evidence — the screen still shows what triggered it, and the
//!    trigger must not fire twice.
//! 3. **A swallowed action is retried, with backoff, a bounded number
//!    of times.**  Input can be lost (the program was starting, the
//!    queue was busy).  Retrying forever would be a hot loop against a
//!    session that is genuinely stuck.
//! 4. **Recovery resets everything.**  The moment the pane does
//!    something, the attempt counter goes back to zero.

use std::time::{Duration, SystemTime};

/// What to type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do this look.
    Nothing,
    /// The rotation finished: clear the context, then start the next.
    ClearAndContinue,
    /// The session stalled on something that was not its fault; ask it
    /// to carry on.
    Continue,
}

/// Why the policy acted — for the log line, and for the tests to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    RotationDone,
    ApiError(&'static str),
    Retry,
}

/// What one look at a pane sees.
pub struct Look<'a> {
    /// The pane's own state machine says it is quiet: the program
    /// finished its turn and the terminal has stopped moving.
    pub quiescent: bool,
    /// Something is running underneath it (a build, a watcher).  Even a
    /// quiet pane with work in flight is not finished.
    pub work_in_flight: bool,
    /// The tail of what is on screen, escapes already stripped.
    pub screen: &'a str,
}

/// Between-look memory for one pane.
#[derive(Debug, Default, Clone)]
pub struct Memory {
    /// What we typed last and when, while we wait to see whether it
    /// took.
    acted: Option<(Action, SystemTime)>,
    /// Consecutive actions without the pane recovering.
    attempts: u32,
    /// When the pane was last seen busy.  A pane that has never been
    /// seen busy is treated as if it just was, so the policy never
    /// fires on its very first look at a session it knows nothing
    /// about.
    last_busy: Option<SystemTime>,
    /// Set once the policy has given up on this pane; cleared by
    /// recovery.  Kept so the giving-up is logged once, not per tick.
    exhausted: bool,
}

impl Memory {
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }
    /// Whether the policy is waiting to see if its last action took.
    pub fn is_waiting(&self) -> bool {
        self.acted.is_some()
    }
}

/// How long a pane must have been quiet before the policy will type.
///
/// On top of what `quiescent` already means (a confirmed quiet state
/// plus 30 s of silent terminal), so the real wait is longer.  The
/// point of the extra margin is the gap between "claude finished
/// printing" and "claude is finished": a session that is about to run
/// one more tool call looks exactly like one that is done.
pub const SETTLE: Duration = Duration::from_secs(20);

/// How long to wait for an action to show an effect before assuming it
/// was swallowed.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(90);

/// Backoff between nudges, indexed by how many have already been made.
///
/// A server that is refusing everyone is not helped by a tighter loop,
/// and the last thing a rate-limited account needs is a retry every
/// thirty seconds for an hour.
pub const BACKOFF: [Duration; 6] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(600),
    Duration::from_secs(900),
];

/// After this many consecutive attempts the policy stops and says so.
/// A session that has not moved after six nudges over half an hour is
/// not going to be rescued by a seventh.
pub const MAX_ATTEMPTS: u32 = 6;

/// Look at a pane and decide what, if anything, to type.
///
/// `now` is passed rather than read so the whole policy is testable at
/// any speed.
pub fn decide(look: &Look, mem: &mut Memory, now: SystemTime) -> (Action, Option<Reason>) {
    // Busy is the answer to almost everything: it means the pane is
    // alive, so anything we were waiting on has happened.
    if !look.quiescent || look.work_in_flight {
        mem.last_busy = Some(now);
        mem.acted = None;
        mem.attempts = 0;
        mem.exhausted = false;
        return (Action::Nothing, None);
    }
    // First sight of a quiet pane: start the clock rather than act on
    // evidence that may predate us entirely.
    let Some(since) = mem.last_busy else {
        mem.last_busy = Some(now);
        return (Action::Nothing, None);
    };
    if now.duration_since(since).unwrap_or_default() < SETTLE {
        return (Action::Nothing, None);
    }
    // Waiting to see whether the last action took.  The pane is still
    // quiet, so it did not — but only after long enough that a slow
    // start would have shown by now.
    if let Some((action, at)) = mem.acted {
        if now.duration_since(at).unwrap_or_default() < ACTION_TIMEOUT {
            return (Action::Nothing, None);
        }
        if mem.attempts >= MAX_ATTEMPTS {
            mem.exhausted = true;
            return (Action::Nothing, None);
        }
        // Backoff applies from the second attempt onward.
        let wait = BACKOFF[(mem.attempts as usize).min(BACKOFF.len() - 1)];
        if now.duration_since(at).unwrap_or_default() < wait {
            return (Action::Nothing, None);
        }
        mem.acted = Some((action, now));
        mem.attempts += 1;
        return (action, Some(Reason::Retry));
    }
    if mem.exhausted {
        return (Action::Nothing, None);
    }
    // An error on screen is the most recent thing that happened, so it
    // wins over a rotation marker further up.
    if let Some(kind) = api_error_kind(look.screen) {
        mem.acted = Some((Action::Continue, now));
        mem.attempts += 1;
        return (Action::Continue, Some(Reason::ApiError(kind)));
    }
    if rotation_finished(look.screen) {
        mem.acted = Some((Action::ClearAndContinue, now));
        mem.attempts += 1;
        return (Action::ClearAndContinue, Some(Reason::RotationDone));
    }
    (Action::Nothing, None)
}

/// Does the screen say the rotation is over and the context can go?
///
/// The wording varies every rotation ("守恒精确)—— 可以 /clear 了",
/// "建议 /clear", "you can /clear now"), so the marker is the command
/// itself.  What makes that safe is the company it keeps: the pane has
/// been quiet a while, nothing runs under it, and the mention is in
/// the last few lines.
///
/// The screen also contains the program's *own* mentions of `/clear`,
/// and those must never fire.  Taken from this pane's real history:
///
/// | line                                            | what it is |
/// |-------------------------------------------------|------------|
/// | `守恒精确)—— 可以 /clear 了`                     | the signal |
/// | `⎿  Tip: Use /clear to start fresh when …`       | a hint the program prints on its own |
/// | `❯ /clear`                                       | the echo of the command being typed |
/// | `/clear (reset)  Start a new session with …`     | the command palette |
///
/// So: drop the program's furniture, drop anything that *is* the
/// command rather than talk about it, and match on what is left.  The
/// tip wraps at the pane's width — it appeared at four different
/// lengths in one log — so it is recognised by its `Tip:` marker, not
/// by its text.
pub fn rotation_finished(screen: &str) -> bool {
    tail_lines(screen, TAIL_LINES)
        .iter()
        .filter(|l| !is_chrome(l))
        .any(|l| l.contains("/clear"))
}

/// Is this line the program's own furniture rather than something it
/// said?
fn is_chrome(line: &str) -> bool {
    let t = line.trim();
    // The command itself: an echo in the input line, a bare retype, or
    // the palette entry that appears while typing `/`.
    if t.starts_with("/clear") {
        return true;
    }
    // A hint the program prints unprompted.
    if t.contains("Tip:") {
        return true;
    }
    // Structural glyphs: tool results (⎿), the input prompt (❯), the
    // spinner line (✻), the mode line (⏵⏵), box rules.
    t.starts_with('⎿')
        || t.starts_with('❯')
        || t.starts_with('✻')
        || t.starts_with('⏵')
        || t.starts_with('│')
        || t.starts_with('─')
        || t.starts_with('╭')
        || t.starts_with('╰')
}

/// How much of the screen counts as "what it just said".
const TAIL_LINES: usize = 12;

/// The kind of API error the pane is *stuck* on, if any.
///
/// Two things on a screen mention API errors and neither is a reason
/// to type anything.  Both were found in this machine's own logs:
///
/// - **The program's own retry.**  `✻ API error · Retrying in 0s ·
///   attempt 1/10` — it retries ten times by itself.  While that line
///   is up it is working, and a nudge is noise at best.  The moment to
///   step in is after those ten are spent.
/// - **A session talking about errors.**  Forty-five of the forty-six
///   "API Error" lines in these logs are prose: a session explaining
///   what an error classifier matches.  A policy that fires on the
///   word would type into any session that discusses its own tooling.
///
/// So the classifier only ever sees lines that are neither furniture
/// nor a retry in progress, and the classifier itself is the
/// conservative one the retry monitor already used: `API Error` plus a
/// recognised kind, not the phrase alone.
pub fn api_error_kind(screen: &str) -> Option<&'static str> {
    let candidates: Vec<&str> = tail_lines(screen, TAIL_LINES)
        .into_iter()
        .filter(|l| !is_chrome(l) && !is_retrying(l))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    super::claudecode::retryable_error_kind(candidates.join("\n").as_bytes())
}

/// Is the program already handling this itself?
fn is_retrying(line: &str) -> bool {
    let t = line.trim();
    t.contains("Retrying in") || (t.contains("attempt ") && t.contains('/'))
}

/// The last `n` non-empty lines, oldest first./// The last `n` non-empty lines, oldest first.
fn tail_lines(s: &str, n: usize) -> Vec<&str> {
    let mut lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn quiet(screen: &str) -> Look<'_> {
        Look { quiescent: true, work_in_flight: false, screen }
    }

    const DONE: &str = "⏺ 本轮做完了,可以 /clear 了\n\n❯";
    const ERROR: &str = "⏺ API Error: 500 Internal server error · Server error\n\n❯";

    /// Bring a pane to the point where the policy is willing to act:
    /// seen busy, then quiet for longer than the settle.
    fn ready(mem: &mut Memory, screen: &str) -> SystemTime {
        decide(&Look { quiescent: false, work_in_flight: false, screen }, mem, t(0));
        t(SETTLE.as_secs() + 1)
    }

    /// The whole point: a finished rotation gets cleared and restarted.
    #[test]
    fn a_finished_rotation_is_cleared_and_restarted() {
        let mut mem = Memory::default();
        let now = ready(&mut mem, DONE);
        let (action, why) = decide(&quiet(DONE), &mut mem, now);
        assert_eq!(action, Action::ClearAndContinue);
        assert_eq!(why, Some(Reason::RotationDone));
    }

    /// Never while the pane is working — a line typed into a busy
    /// session is at best ignored and at worst answered.
    #[test]
    fn nothing_happens_while_the_pane_is_working() {
        let mut mem = Memory::default();
        let now = ready(&mut mem, DONE);
        let busy = Look { quiescent: false, work_in_flight: false, screen: DONE };
        assert_eq!(decide(&busy, &mut mem, now).0, Action::Nothing);
        // …nor while something is running underneath it, however quiet
        // the terminal looks.  A rotation is not over while its build
        // is still going.
        let mut mem = Memory::default();
        let now = ready(&mut mem, DONE);
        let working = Look { quiescent: true, work_in_flight: true, screen: DONE };
        assert_eq!(decide(&working, &mut mem, now).0, Action::Nothing);
    }

    /// A pane must be quiet for a while first: "finished printing" and
    /// "finished" are not the same instant.
    #[test]
    fn a_pane_must_settle_before_anything_is_typed() {
        let mut mem = Memory::default();
        decide(&Look { quiescent: false, work_in_flight: false, screen: DONE }, &mut mem, t(0));
        assert_eq!(decide(&quiet(DONE), &mut mem, t(5)).0, Action::Nothing, "5 s is not settled");
        assert_eq!(
            decide(&quiet(DONE), &mut mem, t(SETTLE.as_secs() + 1)).0,
            Action::ClearAndContinue
        );
    }

    /// The policy must not fire on evidence that predates it.
    ///
    /// A shell restart, a newly adopted pane: the screen may have been
    /// showing "可以 /clear" for an hour, and acting on it immediately
    /// would clear a session someone is in the middle of reading.
    #[test]
    fn the_first_look_at_a_quiet_pane_only_starts_the_clock() {
        let mut mem = Memory::default();
        assert_eq!(decide(&quiet(DONE), &mut mem, t(0)).0, Action::Nothing);
        // …and then the normal settle applies from that first look.
        assert_eq!(decide(&quiet(DONE), &mut mem, t(5)).0, Action::Nothing);
        assert_eq!(
            decide(&quiet(DONE), &mut mem, t(SETTLE.as_secs() + 1)).0,
            Action::ClearAndContinue
        );
    }

    /// One action per quiet period: the screen still shows what
    /// triggered it, and the trigger must not fire twice.
    #[test]
    fn the_same_evidence_does_not_fire_twice() {
        let mut mem = Memory::default();
        let now = ready(&mut mem, DONE);
        assert_eq!(decide(&quiet(DONE), &mut mem, now).0, Action::ClearAndContinue);
        // Screen unchanged a moment later — the session has not even
        // started reacting yet.
        for after in [1u64, 10, 60] {
            let at = now + Duration::from_secs(after);
            assert_eq!(
                decide(&quiet(DONE), &mut mem, at).0,
                Action::Nothing,
                "{after}s after acting"
            );
        }
    }

    /// Input can be lost.  If nothing has changed long after the
    /// action, try once more — with backoff, and not forever.
    #[test]
    fn a_swallowed_action_is_retried_with_backoff_then_given_up_on() {
        let mut mem = Memory::default();
        let mut now = ready(&mut mem, DONE);
        assert_eq!(decide(&quiet(DONE), &mut mem, now).0, Action::ClearAndContinue);
        assert_eq!(mem.attempts(), 1);

        // Retries are spaced by the backoff table, and each one repeats
        // the action that was swallowed.
        for expect in 2..=MAX_ATTEMPTS {
            now += ACTION_TIMEOUT + BACKOFF[BACKOFF.len() - 1];
            let (action, why) = decide(&quiet(DONE), &mut mem, now);
            assert_eq!(action, Action::ClearAndContinue, "attempt {expect}");
            assert_eq!(why, Some(Reason::Retry));
            assert_eq!(mem.attempts(), expect);
        }
        // And then it stops, once, loudly.
        now += ACTION_TIMEOUT + BACKOFF[BACKOFF.len() - 1];
        assert_eq!(decide(&quiet(DONE), &mut mem, now).0, Action::Nothing);
        assert!(mem.is_exhausted(), "the policy has to admit it is stuck");
    }

    /// Recovery resets everything: the pane moved, so whatever we were
    /// waiting on happened.
    #[test]
    fn the_pane_doing_something_resets_the_policy() {
        let mut mem = Memory::default();
        let now = ready(&mut mem, DONE);
        decide(&quiet(DONE), &mut mem, now);
        assert_eq!(mem.attempts(), 1);
        assert!(mem.is_waiting());

        let busy = Look { quiescent: false, work_in_flight: false, screen: DONE };
        decide(&busy, &mut mem, now + Duration::from_secs(5));
        assert_eq!(mem.attempts(), 0, "it moved — nothing is outstanding");
        assert!(!mem.is_waiting());
    }

    /// While the program is retrying on its own, nothing is wrong that
    /// a nudge would fix.
    ///
    /// Verbatim from the logs: claude retries ten times by itself, and
    /// says so.  Typing into that is noise at best; the moment to step
    /// in is after those ten are spent.
    #[test]
    fn its_own_retry_is_not_our_business() {
        let screen = "✻ API error · Retrying in 0s · attempt 1/10";
        assert_eq!(api_error_kind(screen), None);
        let mut mem = Memory::default();
        let now = ready(&mut mem, screen);
        assert_eq!(decide(&quiet(screen), &mut mem, now).0, Action::Nothing);
    }

    /// A session *talking* about errors is not a session having one.
    ///
    /// Forty-five of the forty-six "API Error" lines in this machine's
    /// logs are exactly this: prose about an error classifier.  A
    /// policy that fired on the word would have typed into every
    /// session that discusses its own tooling — including the one that
    /// wrote this policy.
    #[test]
    fn prose_about_errors_is_not_an_error() {
        let screen = "⏺ 现有的 retryable_error_kind 分类器还在(匹配 claude 的 API Error: … · \n                      <kind> 那个形状),当年缺的只是拿到字节的路。";
        assert_eq!(api_error_kind(screen), None);
    }

    /// A server error is not the session's fault: nudge it to carry
    /// on, without clearing anything.
    #[test]
    fn a_server_error_gets_a_plain_continue() {
        let mut mem = Memory::default();
        let now = ready(&mut mem, ERROR);
        let (action, why) = decide(&quiet(ERROR), &mut mem, now);
        assert_eq!(action, Action::Continue, "never /clear on an error");
        assert_eq!(why, Some(Reason::ApiError("server_error")));
    }

    /// An error after a rotation marker wins: it is the more recent
    /// thing that happened, and clearing on it would throw away the
    /// context the retry needs.
    #[test]
    fn an_error_takes_precedence_over_a_rotation_marker() {
        let screen = format!("{DONE}\n{ERROR}");
        let mut mem = Memory::default();
        let now = ready(&mut mem, &screen);
        assert_eq!(decide(&quiet(&screen), &mut mem, now).0, Action::Continue);
    }

    /// The rotation marker is the command, not a phrase — the wording
    /// varies between rotations and between languages.
    #[test]
    fn the_rotation_marker_is_the_command_itself() {
        for said in [
            // Verbatim from this pane's own history.
            "守恒精确)—— 可以 /clear 了",
            "pass 零回归,TRIG 全 PASS)—— 可以 /clear 了。",
            "建议现在 /clear,然后继续",
            "This rotation is done — you can /clear now.",
        ] {
            assert!(rotation_finished(said), "{said:?}");
        }
        assert!(!rotation_finished("still working on the clear-cache path"));
        assert!(!rotation_finished(""));
    }

    /// The program's own mentions of `/clear` must never fire.
    ///
    /// Every one of these was on this pane's screen while it was
    /// working — the first version of the matcher would have cleared a
    /// live session on any of them.
    #[test]
    fn the_programs_own_furniture_is_not_an_instruction() {
        for chrome in [
            // The hint it prints unprompted — and it wraps at the
            // pane's width, so it appeared at four different lengths
            // in one log.  Recognised by its marker, not its text.
            "⎿  Tip: Use /clear to start fresh when switching topics and free up",
            "⎿  Tip: Use /clear to start fresh when switching",
            "⎿  Tip: Use /clear to start fresh when",
            // The echo of the command being typed — including ours.
            "❯ /clear",
            "/clear",
            // The command palette, open while someone types `/`.
            "/clear (reset)              Start a new session with empty context;",
        ] {
            assert!(!rotation_finished(chrome), "{chrome:?} must not fire");
        }
        // …and a real message still fires when it sits among them.
        let screen = "⏺ rotation 272 收官(gate 2304/0/4,双 sweep\n                      守恒精确)—— 可以 /clear 了\n                      ⎿  Tip: Use /clear to start fresh when switching topics\n                      ❯";
        assert!(rotation_finished(screen), "the signal survives the furniture");
    }

    /// …and it has to be recent.  A `/clear` said fifty lines ago is
    /// scrollback, not an instruction.
    #[test]
    fn an_old_mention_of_clear_is_not_a_trigger() {
        let mut screen = String::from("⏺ 上一轮说过可以 /clear\n");
        for i in 0..40 {
            screen.push_str(&format!("line {i}\n"));
        }
        assert!(!rotation_finished(&screen), "it has scrolled out of what it just said");
    }
}
