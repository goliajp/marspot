//! Where each claude or kimi helper's turn is, and which of its ends are
//! told to the parent: by claude's own record of them, else by the
//! per-pane state the shell samples.

use std::collections::HashMap;
use std::time::Duration;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::Shared;
use super::notify::Stop;
use super::table::{Event, Helper, Table};
use super::tick::START_GRACE_MS;
use super::work::Job;
use crate::plugins::PluginHost;
use crate::plugins::claudecode::with_helpers::turns::{self, Look};

/// How long after a screen at a prompt could not be read it is read
/// again: a pane whose screen stays unreadable is not read every tick.
pub(super) const REREAD_MS: u64 = 5_000;
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
    /// The same, told before the shell started: the prompt it is at now
    /// may be another.
    BlockedBefore,
}

/// Whether the table says `h` is in a turn that began after its last
/// stop was told: given to it some other way than as input seen here.
pub(super) fn in_later_turn(h: &Helper) -> bool {
    let told = h.notices.last().copied();
    turn_reported(h) && h.clock.since_ms.is_some_and(|since| told.is_some_and(|t| t < since))
}

/// The turns a start begins with.  A helper whose task went in and
/// whose last stop was not told may have ended its turn while no plugin
/// ran, or had its stop lost with the queue: it counts as working, so
/// that its wait for input is told.  One with a stop held back by the
/// rate is told of that one.  One last told of at an approval prompt
/// may still be at it.  One in a turn after the stop that was told is
/// in it until it is seen to have ended.
pub(super) fn turns_at_start(t: &Table) -> HashMap<u64, Turn> {
    let turn = |h: &Helper| {
        let untold = h.delivered_ms.is_some() && !h.noticed && h.suppressed == 0;
        if untold || in_later_turn(h) { Some(Turn::Working) } else { h.blocked_told.then_some(Turn::BlockedBefore) }
    };
    t.map.values().filter(|h| turn_reported(h)).filter_map(|h| Some((h.sid, turn(h)?))).collect()
}

/// Whether `ev` is something given to `h` after a stop of its that was
/// told: its next turn may be about to start, or may not.
pub(super) fn pokes(h: &Helper, ev: &Event) -> bool {
    turn_reported(h) && h.noticed && matches!(ev, Event::InputSeen | Event::ResultChanged)
}

/// The helpers [`pokes`] was true of, as far as the table of a start
/// says: one in a turn shows it soon.
pub(super) fn poked_at_start(t: &Table, now_ms: u64) -> HashMap<u64, u64> {
    let told_before = t.map.values().filter(|h| turn_reported(h) && !h.noticed && !h.notices.is_empty() && h.suppressed == 0);
    told_before.map(|h| (h.sid, now_ms)).collect()
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

    pub(super) fn at_work(&self, sid: u64, yes: bool) {
        let mut st = self.lock();
        let changed = if yes { st.at_work.insert(sid) } else { st.at_work.remove(&sid) };
        drop(st);
        if changed {
            self.strip_changed();
        }
    }

    /// Whether `h`, with nothing new to show, waits for helpers of its
    /// own: one still runs, or the last was told of or went a moment ago
    /// (a notice starts `h`'s next turn once it is read).
    fn awaits_helpers(&self, t: &Table, h: &Helper, now_ms: u64) -> bool {
        if h.result_new {
            return false;
        }
        let recent = |at: u64| now_ms.saturating_sub(at) < AFTER_HELPERS_MS;
        let st = self.lock();
        // told before, and what it was given since opened no turn
        let still_stopped = |c: &Helper| st.poked.get(&c.sid).is_some_and(|at| !recent(*at));
        t.under(h.sid).any(|c| !c.noticed && !still_stopped(c) || c.notices.last().is_some_and(|at| recent(*at)))
            || st.child_gone.get(&h.sid).is_some_and(|at| recent(*at))
    }
}

/// Agents whose plugin reports turns: their stops come from the tick.
pub(super) fn turn_reported(h: &Helper) -> bool {
    !h.contained() && matches!(h.started.agent, AgentKind::Claude | AgentKind::Kimi)
}

/// A claude or kimi helper's turn ended (it waits for input), it waits
/// on an approval, or its program left the pane.
pub(super) fn turn(s: &Shared, host: &dyn PluginHost, t: &Table, h: &Helper, now_ms: u64, in_turn: &mut HashMap<u64, Turn>) {
    let Ok(Some(mut v)) = host.pane_status(h.sid) else { return };
    let claude = h.started.agent == AgentKind::Claude;
    let look = if claude && v.status != PaneStatus::Empty { turns::look(h.sid) } else { None };
    let awaiting = PaneStatus::Busy(BusyKind::ToolAwaitingApproval);
    // its record and its screen say so before the pane is quiet long
    // enough for its state to
    let read = |since| s.queue(Job::ReadPrompt { sid: h.sid, since });
    if claude && v.status != awaiting && s.at_prompt(h.sid, look, now_ms, drawn_ms(h.sid, now_ms), read) {
        v.status = awaiting;
        v.held = v.held.max(BLOCKED_AFTER);
    }
    // in a turn, not only printing
    let at_work = matches!(v.status, PaneStatus::Busy(BusyKind::Working | BusyKind::ToolExecuting | BusyKind::ToolAwaitingApproval));
    // over by its own record, it is not, whatever it still draws
    s.at_work(h.sid, at_work && !matches!(look, Some(Look::Over { .. })));
    if at_work {
        let mut st = s.lock();
        st.turned.insert(h.sid);
        // an approval prompt may be the one it stopped at before
        if v.status != PaneStatus::Busy(BusyKind::ToolAwaitingApproval) {
            st.poked.remove(&h.sid);
        }
    }
    let seen = in_turn.entry(h.sid).or_default();
    // a prompt whose screen could not be read is asked about again
    let mut st = s.lock();
    if v.status != PaneStatus::Busy(BusyKind::ToolAwaitingApproval) {
        st.prompt_unread.remove(&h.sid);
    } else if let Some(again) = st.prompt_unread.get_mut(&h.sid)
        && *seen == Turn::BlockedTold
        && *again <= now_ms
    {
        // queued: the read moves it on
        *again = u64::MAX;
        *seen = Turn::BlockedBefore;
    }
    drop(st);
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
        PaneStatus::Busy(BusyKind::ToolAwaitingApproval) if v.held >= BLOCKED_AFTER && *seen == Turn::BlockedBefore => {
            if !queue(s, h.sid, seen, Turn::BlockedTold, Job::StillBlocked { sid: h.sid })
                && let Some(again) = s.lock().prompt_unread.get_mut(&h.sid)
            {
                *again = now_ms;
            }
        }
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

/// When pane `sid` last printed, as its session last said (ms after the
/// epoch, `now_ms` now); None when it has not said.
fn drawn_ms(sid: u64, now_ms: u64) -> Option<u64> {
    let at = crate::control_reader::pane_wakes::latest(sid)?.output_at?;
    Some(now_ms.saturating_sub(at.elapsed().as_millis() as u64))
}

/// Note the turn as `now` and queue its stop; a stop that did not go
/// leaves the turn as it was, for the next tick.
fn queue_stop(s: &Shared, sid: u64, seen: &mut Turn, now: Turn, stop: Stop) -> bool {
    queue(s, sid, seen, now, Job::Stopped { sid, stop, by_screen: false })
}

fn queue(s: &Shared, sid: u64, seen: &mut Turn, now: Turn, job: Job) -> bool {
    let name = job.name();
    let was = std::mem::replace(seen, now);
    let queued = s.queue(job);
    if !queued {
        *seen = was;
        marspot::lx_warn!("helpers.queue_full", &format!("sid={sid}: {name} not queued"));
    }
    queued
}
