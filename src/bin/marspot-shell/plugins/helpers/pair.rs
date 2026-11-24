//! Which two panes hold a helper and the session that started it, and
//! what becomes of that when the parent pane is handed to another agent.

use std::time::Duration;

use marspot::pidtree::ProcId;

use super::table::{Event, Helper, Table};
use super::{SHARED, Shared, records};
use crate::plugins::peerbus::{Conversation, conversation_of};

/// Whether one of two panes is a helper started by the conversation the
/// other is in now, or by one that was handed over to it.
pub(in crate::plugins) fn is_pair(a: u64, b: u64, conversation_of: impl Fn(u64) -> Option<Conversation>) -> bool {
    SHARED.get().is_some_and(|s| pair_in(s, a, b, conversation_of))
}

/// A pane outlives the conversation in it: another one there did not
/// start the helper.  The helper's own pane needs no such look, as it is
/// closed when its agent leaves it.
fn pair_in(s: &Shared, a: u64, b: u64, conversation_of: impl Fn(u64) -> Option<Conversation>) -> bool {
    let of_pair = |t: &Table| t.helper_of_pair(a, b).cloned();
    let Some(h) = of_pair(&s.table()) else { return false };
    if h.conv.is_none() {
        return true;
    }
    let now = conversation_of(h.started.parent);
    settle(s, h.started.parent, now.as_ref());
    of_pair(&s.table()).is_some_and(|h| now.is_some_and(|c| h.conv == Some(c.id)))
}

/// The helpers waiting for the agent their parent was handed to follow
/// the conversation `now`, if that agent is in one.
fn settle(s: &Shared, parent: u64, now: Option<&Conversation>) {
    let Some(c) = now else { return };
    let follow: Vec<u64> = s.table().under(parent).filter(|h| h.heir == Some(c.agent)).map(|h| h.sid).collect();
    for sid in follow {
        s.record(sid, Event::ConversationChanged(c.id.clone()));
    }
}

/// [`settle`] with a look at the parent pane, when a helper waits on it.
pub(super) fn settle_now(s: &Shared, parent: u64) {
    if s.table().under(parent).any(|h| h.heir.is_some()) {
        settle(s, parent, conversation_of(parent).as_ref());
    }
}

/// A pane handed from one agent to another.
#[derive(Debug)]
pub(in crate::plugins) struct Moved {
    pub pane: u64,
    /// The conversation it left, and the process that held it.
    pub from: Option<String>,
    pub left: ProcId,
    /// The conversation the new agent goes on with; None: a new one,
    /// which nothing names yet.
    pub resumed: Option<String>,
}

/// The conversation pane `sid` is in, asked before its agent is taken
/// down; None when no helper depends on it.
pub(in crate::plugins) fn leaving(sid: u64) -> Option<String> {
    let s = SHARED.get()?;
    s.table().under(sid).next()?;
    let now = conversation_of(sid);
    settle(s, sid, now.as_ref());
    now.map(|c| c.id)
}

/// The new agent of `m` is up: noted on the main loop, written down by
/// the worker.
pub(in crate::plugins) fn moved(m: Moved) {
    if let Some(s) = SHARED.get() {
        note_move(s, m, records::now_ms() * 1000);
    }
}

/// Note move `m`, which happened at `at_us` (µs after the epoch), with
/// the helpers that follow it as the table says now.
pub(super) fn note_move(s: &Shared, m: Moved, at_us: u64) {
    let sids: Vec<u64> = follows(&s.table(), &m).map(|h| h.sid).collect();
    s.lock().moves.add(m, sids, at_us);
    s.wake();
}

/// The helpers of the conversation `m` left.
fn follows<'a>(t: &'a Table, m: &'a Moved) -> impl Iterator<Item = &'a Helper> {
    t.under(m.pane).filter(|h| h.conv.is_some() && (h.conv == m.from || h.heir == Some(m.left)))
}

/// A move not yet written down: the helpers that follow it, and when it
/// happened (µs after the epoch).
#[derive(Debug)]
pub(super) struct Pending {
    pub m: Moved,
    pub sids: Vec<u64>,
    pub at_us: u64,
}

/// The moves not yet written down, oldest first.  None is dropped: a
/// move that goes on from one still here takes that one's helpers along,
/// and a helper is in one move only, so there are no more than helpers.
#[derive(Debug, Default)]
pub(super) struct Moves(Vec<Pending>);

impl Moves {
    fn add(&mut self, m: Moved, mut sids: Vec<u64>, at_us: u64) {
        for p in self.0.iter_mut().filter(|p| p.m.pane == m.pane && goes_on_from(p, &m)) {
            sids.append(&mut p.sids);
        }
        sids.sort_unstable();
        sids.dedup();
        for p in &mut self.0 {
            p.sids.retain(|sid| !sids.contains(sid));
        }
        self.0.retain(|p| !p.sids.is_empty());
        if !sids.is_empty() {
            self.0.push(Pending { m, sids, at_us });
        }
    }
}

/// Whether `m` leaves what `p` handed the pane to: the conversation it
/// resumed, or an agent that was running when it happened.
fn goes_on_from(p: &Pending, m: &Moved) -> bool {
    let resumed = p.m.resumed.is_some() && m.from == p.m.resumed;
    resumed || (m.left != p.m.left && m.left.start_us <= p.at_us)
}

/// What the worker sees in front of a pane: the agent, and the
/// conversation of the agent there.
#[derive(Debug, Default)]
pub(super) struct Look {
    pub front: Option<ProcId>,
    pub conv: Option<Conversation>,
}

/// The pane as it is now.
pub(super) fn look_now(pane: u64) -> Look {
    let obs = crate::observe::shared().fresh(Duration::ZERO);
    let front = obs.session(pane).and_then(|x| x.front()).map(|p| p.row.id());
    Look { front, conv: front.and_then(|_| conversation_of(pane)) }
}

/// Write down the moves noted, each with what `look` sees of its pane.
pub(super) fn write_moves(s: &Shared, look: impl Fn(u64) -> Look) {
    let pending = std::mem::take(&mut s.lock().moves.0);
    for p in pending {
        parent_moved(s, &p, &look(p.m.pane));
    }
}

/// Write down what the helpers of `p` follow, with `look` of its pane.
/// Only an agent that was running when the move happened is the one
/// it was handed to, and the conversation it is in now is the one it
/// goes on with: one it moves to later (a `/clear`) is another.
pub(super) fn parent_moved(s: &Shared, p: &Pending, look: &Look) {
    let front = look.front.filter(|f| *f != p.m.left && f.start_us <= p.at_us);
    let ev = match (&p.m.resumed, front) {
        (Some(c), _) => Event::ConversationChanged(c.clone()),
        (None, Some(f)) => match look.conv.as_ref().filter(|c| c.agent == f) {
            Some(c) => Event::ConversationChanged(c.id.clone()),
            None => Event::ParentAgent(f),
        },
        (None, None) => {
            marspot::lx_warn!("helpers.parent_moved", &format!("sid={}: no agent in front that was there at the move", p.m.pane));
            return;
        }
    };
    for sid in &p.sids {
        if s.table().under(p.m.pane).any(|h| h.sid == *sid) {
            s.record(*sid, ev.clone());
        }
    }
}

#[cfg(test)]
#[path = "tests_pair.rs"]
mod tests;
