//! What to do with a pane its account has refused: whether it waits,
//! and, for an agent whose choice of account is not the allocation
//! core's yet, where it goes.

use std::time::Duration;

use super::{best_profile, Refusal, Room};

/// Everything the decision depends on.
///
/// Gathered by the caller, decided here: one place says what happens
/// to a refused pane, and it can be read as a table.
pub(in crate::plugins) struct Situation<'a> {
    /// What the feed says about every profile on the machine.
    pub rooms: &'a [(u8, Room)],
    /// The profile the pane is on — the one that just refused it.
    pub current: u8,
    /// A tool is executing on this machine right now.  Not "the agent
    /// is busy" — an agent stranded on an account that refuses it is
    /// not busy, it is stuck, and waiting for it to finish waits
    /// forever.  This is the narrower thing: a command of the user's
    /// is running, and killing the process would take it with it.
    pub tool_executing: bool,
    /// claude has a process of its own running under it that is not a
    /// resting helper -- a background job, a watcher -- or the scan
    /// could not tell.  A move kills it too.
    pub work_running: bool,
    /// Moves already made while this pane has been continuously
    /// refused.
    pub moves_this_episode: u8,
    /// Since the last move, if there was one.
    pub since_last_move: Option<Duration>,
    /// How long this pane has been stuck on an account that is out.
    pub stranded_for: Option<Duration>,
    /// The session was told, while this account could still answer,
    /// that it was about to run out, and it had the last of the
    /// account to bring its work to a point it can be stopped at.
    pub warned: bool,
    /// Where the last move was aimed, if there was one.  A pane is
    /// bound to a profile by the scan, which happens after the
    /// process it names exists — so between the move and the binding
    /// the pane still reads as being on the account it is leaving.
    pub last_target: Option<u8>,
    /// The model this pane is working in, when the feed caps it
    /// separately.
    ///
    /// Some projects can only run on Fable. Fable carries a quota of
    /// its own, and it is bounded by the account's 5h and 7d windows
    /// as well -- so a profile is usable for such a pane only when
    /// both the account and the model have room. For every other pane
    /// this is `None` and nothing about the decision changes.
    pub needs_model: Option<&'a str>,
    /// The newest thing a background agent of this session reported is
    /// that the API refused it for quota, under the claude on this
    /// account.
    pub background_refused: Option<&'a Refusal>,
}

/// What to do with a pane its account has refused.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::plugins) enum Move {
    /// Move it to this profile.
    To(u8),
    /// Leave it where it is, for this reason — worth a log line and
    /// worth asserting on.
    Hold(&'static str),
}

/// Two moves per refusal: the second corrects the first.
pub(in crate::plugins) const MOVES_PER_EPISODE: u8 = 2;
/// And then it waits.
pub(in crate::plugins) const MOVE_COOLDOWN: Duration = Duration::from_secs(600);
/// How long "a tool is running here", or work of claude's own in the
/// background, is allowed to hold a pane.
///
/// The hold exists to protect a command of the user's from being
/// killed with the process.  It is not meant to cover a CLI that has
/// parked itself until its window reopens, which reads the same from
/// the transcript — and did, for nine hours and 170 log lines
/// (2026-09-29, sid 430).  Past this, the pane moves.
///
/// Five minutes, not ten: the account cannot answer whatever the work
/// comes back with, so a longer wait buys a finished command whose
/// result nobody reads, at the price of a pane standing still.  What
/// it costs is a foreground command between five and ten minutes long
/// cut off; the moved session is asked to carry on and runs it again.
pub(in crate::plugins) const TOOL_HOLD_MAX: Duration = Duration::from_secs(300);

/// Why a refused pane waits where it is, or `None` when it may move
/// now: only where it goes is left to choose.
///
/// Mechanical on purpose: no clock of its own, no filesystem, no
/// side effect.  The caller gathers, this decides, the caller acts.
pub(in crate::plugins) fn hold_for(s: &Situation) -> Option<Move> {
    // Moving kills the process, and a command running under it goes
    // with it.  That one waits; it will finish, and the next pass will
    // move the pane.
    //
    // Work in the background is held for the same while and no longer.
    // It used to hold the pane for as long as it ran, and a session
    // keeps such things for hours -- a loop polling for a file, a
    // watchdog -- so a pane whose account had run out sat on it, idle,
    // until someone moved it by hand (pane 482, 2026-10-17).  The
    // account cannot answer whatever that work comes back with.
    //
    // A session that was told in time has already done what the wait
    // was for, so its background work no longer holds it: the account
    // cannot answer anything that work comes back with, and the pane
    // only stands still. A command in the foreground still does, until
    // it ends -- cut off halfway it can leave a write half done.
    //
    // Nor does background work the account has stopped answering. An
    // agent in the background needs the account for every step, and
    // one that has just been refused on a model this account is out
    // for is the evidence that such work is failing, not running: the
    // processes still under claude belong to agents about to meet the
    // same refusal, each failure waking a conversation that cannot
    // answer either (four panes, 2026-11-11, two to five minutes
    // each). An agent refused on a model the account still serves is
    // not that evidence, and no report of a refusal is no evidence.
    let work_stuck = s.background_refused.is_some_and(|r| {
        let model = (!r.model.is_empty()).then_some(r.model.as_str());
        s.rooms.iter().any(|(p, room)| *p == s.current && room.refused_for(model))
    });
    let work_holds = s.work_running && !s.warned && !work_stuck;
    if (s.tool_executing || work_holds)
        && s.stranded_for.map(|d| d < TOOL_HOLD_MAX).unwrap_or(true)
    {
        return Some(Move::Hold(if s.tool_executing {
            "a tool is running here"
        } else {
            "work is in flight here"
        }));
    }
    // A move already made and not yet visible.  The pane is rebound by
    // the scan, and the scan can only bind it once the new process is
    // there — until then it still reads as sitting on the account it
    // is leaving, and moving again restarts a resume that is halfway
    // through.  The caller has to keep this tally across the gap where
    // the pane has no binding at all, which is exactly where it was
    // being dropped (pane 442, 2026-09-29: three cycles in 40 seconds).
    if let Some(t) = s.last_target
        && t != s.current {
            return Some(Move::Hold("the last move has not landed"));
        }
    if s.moves_this_episode >= MOVES_PER_EPISODE
        && s.since_last_move.map(|d| d < MOVE_COOLDOWN).unwrap_or(false)
    {
        return Some(Move::Hold("cooling down"));
    }
    None
}

/// Where a refused pane goes next, or why it stays, by `best_profile`'s
/// ranking: Kimi's choice until it has an allocation of its own.
pub(in crate::plugins) fn next_move(s: &Situation) -> Move {
    if let Some(hold) = hold_for(s) {
        return hold;
    }
    let Some(target) = best_profile(s.rooms, s.current, s.needs_model) else {
        return Move::Hold("nowhere else to go");
    };
    // Every account is out.  That is a normal ending: the CLI is
    // already waiting for its own window to reopen, and moving would
    // trade one closed door for another.
    let known_shut = s
        .rooms
        .iter()
        .find(|(p, _)| *p == target)
        .map(|(_, r)| r.refused_for(s.needs_model))
        .unwrap_or(false);
    if known_shut {
        return Move::Hold("every account is out");
    }
    Move::To(target)
}
