//! Where each claude or kimi helper's turn is, and which of its ends are
//! told to the parent: by claude's own record of them, else by the
//! per-pane state the shell samples.

use std::collections::HashMap;
use std::time::Duration;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::Shared;
use super::notify::Stop;
use super::table::{Helper, Table};
use super::tick::START_GRACE_MS;
use super::work::Job;
use crate::plugins::PluginHost;
use crate::plugins::claudecode::with_helpers::turns::{self, Look};

/// Waiting on an approval this long is a stop at a prompt.
const BLOCKED_AFTER: Duration = Duration::from_secs(3);
/// How long after the last of its helpers was told of or went a helper's
/// end is still not told: its next turn may be about to start.
pub(super) const AFTER_HELPERS_MS: u64 = 10_000;

/// Where a claude or kimi helper's turn is, since its last stop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Turn {
    /// No turn seen since the last stop was told.
    #[default]
    Idle,
    Working,
    /// Over by claude's own record; not told while its helpers run.
    Held,
    /// Stopped at an approval prompt, and that was told.
    BlockedTold,
}

/// The turns a start begins with.  A helper whose task went in and
/// whose last stop was not told may have ended its turn while no plugin
/// ran, or had its stop lost with the queue: it counts as working, so
/// that its wait for input is told.  One with a stop held back by the
/// rate is told of that one.
pub(super) fn turns_at_start(t: &Table) -> HashMap<u64, Turn> {
    let untold = t.map.values().filter(|h| turn_reported(h) && h.delivered_ms.is_some() && !h.noticed && h.suppressed == 0);
    untold.map(|h| (h.sid, Turn::Working)).collect()
}

/// The last turn's end of a claude helper that was told.
#[derive(Clone, Copy, Debug)]
pub(super) struct Told {
    /// When it was (ms after the epoch).
    pub at: u64,
    /// Judged from the pane's state, with no sign of another turn since:
    /// claude's record of that same end may still come.
    pub by_state: bool,
}

impl Shared {
    /// The last turn's end of `h` that its parent was told of, or need
    /// not be: one before it was started (a conversation it goes on
    /// with) or before its last notice; a stop the rate holds back is
    /// told when the rate lets it.
    fn turn_told(&self, h: &Helper, now_ms: u64) -> Told {
        let first = || match h.suppressed {
            0 => h.notices.iter().copied().fold(h.started_ms, u64::max),
            _ => now_ms,
        };
        *self.lock().turn_told.entry(h.sid).or_insert_with(|| Told { at: first(), by_state: false })
    }

    /// Whether `h`, with nothing new to show, waits for helpers of its
    /// own: one still runs, or the last was told of or went a moment ago
    /// (a notice starts `h`'s next turn once it is read).
    fn awaits_helpers(&self, t: &Table, h: &Helper, now_ms: u64) -> bool {
        if h.result_new {
            return false;
        }
        let recent = |at: u64| now_ms.saturating_sub(at) < AFTER_HELPERS_MS;
        t.under(h.sid).any(|c| !c.noticed || c.notices.last().is_some_and(|at| recent(*at)))
            || self.lock().child_gone.get(&h.sid).is_some_and(|at| recent(*at))
    }
}

/// Agents whose plugin reports turns: their stops come from the tick.
pub(super) fn turn_reported(h: &Helper) -> bool {
    matches!(h.started.agent, AgentKind::Claude | AgentKind::Kimi)
}

/// A claude or kimi helper's turn ended (it waits for input), it waits
/// on an approval, or its program left the pane.
pub(super) fn turn(s: &Shared, host: &dyn PluginHost, t: &Table, h: &Helper, now_ms: u64, in_turn: &mut HashMap<u64, Turn>) {
    let Ok(Some(v)) = host.pane_status(h.sid) else { return };
    // in a turn, not only printing
    let at_work = matches!(v.status, PaneStatus::Busy(BusyKind::Working | BusyKind::ToolExecuting | BusyKind::ToolAwaitingApproval));
    if at_work {
        s.lock().turned.insert(h.sid);
    }
    let seen = in_turn.entry(h.sid).or_default();
    let claude = h.started.agent == AgentKind::Claude;
    let look = if claude && v.status != PaneStatus::Empty { turns::look(h.sid) } else { None };
    if let Some(Look::Over { began, at, quiet_after }) = look {
        if at > h.started_ms {
            s.lock().turned.insert(h.sid);
        }
        return recorded_end(s, t, h, (began, at, quiet_after), now_ms, seen);
    }
    if at_work
        && let Some(told) = s.lock().turn_told.get_mut(&h.sid)
    {
        told.by_state = false;
    }
    match v.status {
        PaneStatus::Busy(BusyKind::ToolAwaitingApproval) if v.held >= BLOCKED_AFTER && *seen != Turn::BlockedTold => {
            queue_stop(s, h.sid, seen, Turn::BlockedTold, Stop::Blocked);
        }
        PaneStatus::Busy(BusyKind::ToolAwaitingApproval) => {}
        // still drawing the end that is held, unless a scan saw more
        PaneStatus::Busy(_) if *seen == Turn::Held && look.is_none() => {}
        PaneStatus::Busy(_) => *seen = Turn::Working,
        PaneStatus::AwaitingUser if v.quiescent && *seen != Turn::Idle => {
            if *seen == Turn::Held && s.awaits_helpers(t, h, now_ms) {
                return;
            }
            if queue_stop(s, h.sid, seen, Turn::Idle, Stop::Idle) && claude {
                s.lock().turn_told.insert(h.sid, Told { at: now_ms, by_state: true });
            }
        }
        PaneStatus::Empty if v.quiescent && h.delivered_ms.is_some_and(|d| now_ms.saturating_sub(d) > START_GRACE_MS) => {
            s.queue_closing(h.sid, Job::Stopped { sid: h.sid, stop: Stop::Exited, by_screen: false });
        }
        _ => {}
    }
}

/// The turn of `h` is over by claude's own record, whatever it still
/// draws: it began and ended as `turn` says.
fn recorded_end(s: &Shared, t: &Table, h: &Helper, turn: (Option<u64>, u64, Option<u64>), now_ms: u64, seen: &mut Turn) {
    let (began, at, quiet_after) = turn;
    let told = s.turn_told(h, now_ms);
    let told_now = Told { at, by_state: false };
    if at <= told.at {
        *seen = Turn::Idle;
    } else if told.by_state && began.is_none_or(|b| b <= told.at) {
        // the record of the end the pane's state already told
        *seen = Turn::Idle;
        s.lock().turn_told.insert(h.sid, told_now);
    } else if quiet_after.is_some_and(|before| before <= told.at) && !h.notices.is_empty() && !h.result_new {
        *seen = Turn::Idle;
        s.lock().turn_told.insert(h.sid, told_now);
        let why = "its turn read a notice about a helper and did nothing; its stop before that was told";
        marspot::lx_info!("helpers.stop_not_told", &format!("sid={}: {why}", h.sid));
    } else if s.awaits_helpers(t, h, now_ms) {
        // looked at again each tick: helpers may go with no notice
        *seen = Turn::Held;
    } else if queue_stop(s, h.sid, seen, Turn::Idle, Stop::Idle) {
        s.lock().turn_told.insert(h.sid, told_now);
    }
}

/// Note the turn as `now` and queue its stop; a stop that did not go
/// leaves the turn as it was, for the next tick.
fn queue_stop(s: &Shared, sid: u64, seen: &mut Turn, now: Turn, stop: Stop) -> bool {
    let was = std::mem::replace(seen, now);
    let queued = s.queue(Job::Stopped { sid, stop, by_screen: false });
    if !queued {
        *seen = was;
        marspot::lx_warn!("helpers.queue_full", &format!("sid={sid}: {stop:?} not queued"));
    }
    queued
}
