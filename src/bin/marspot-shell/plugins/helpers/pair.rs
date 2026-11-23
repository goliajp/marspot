//! Which two panes hold a helper and the session that started it, and
//! what becomes of that when the parent pane is handed to another agent.

use marspot::pidtree::ProcId;

use super::table::{Event, Helper, Table};
use super::work::Job;
use super::{SHARED, Shared};
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

/// The new agent of `m` is up.
pub(in crate::plugins) fn moved(m: Moved) {
    let Some(s) = SHARED.get() else { return };
    let pane = m.pane;
    if follows(&s.table(), &m).next().is_some() && !s.queue(Job::ParentMoved(m)) {
        marspot::lx_warn!("helpers.not_queued", &format!("sid={pane} parent_moved"));
    }
}

/// The helpers of the conversation `m` left.
fn follows<'a>(t: &'a Table, m: &'a Moved) -> impl Iterator<Item = &'a Helper> {
    t.under(m.pane).filter(|h| h.conv.is_some() && (h.conv == m.from || h.heir == Some(m.left)))
}

/// Write down what the helpers of `m`'s pane follow; `front` is the
/// agent in front of it now.
pub(super) fn parent_moved(s: &Shared, m: &Moved, front: Option<ProcId>) {
    let ev = match (&m.resumed, front) {
        (Some(c), _) => Event::ConversationChanged(c.clone()),
        (None, Some(p)) => Event::ParentAgent(p),
        (None, None) => {
            marspot::lx_warn!("helpers.parent_moved", &format!("sid={}: no agent in front", m.pane));
            return;
        }
    };
    let sids: Vec<u64> = follows(&s.table(), m).map(|h| h.sid).collect();
    for sid in sids {
        s.record(sid, ev.clone());
    }
}

#[cfg(test)]
#[path = "tests_pair.rs"]
mod tests;
