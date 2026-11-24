//! Which two panes hold a helper and the session that started it, and
//! what becomes of that when the parent pane is handed to another agent.

use marspot::pidtree::{ProcId, ProcRow};

use super::table::{Event, Helper, Table};
use super::{SHARED, Shared};
use crate::plugins::peerbus::{Conversation, conversation_of};

/// Whether one of two panes is a helper started by the conversation the
/// other is in now, or by one that was handed over to it.
pub(in crate::plugins) fn is_pair(a: u64, b: u64, conversation_of: impl Fn(u64) -> Option<Conversation>) -> bool {
    SHARED.get().is_some_and(|s| pair_in(s, a, b, conversation_of))
}

/// A pane outlives the conversation in it: another one there did not
/// start the helper.  The helper's own pane needs no such look, as it is
/// closed when its agent leaves it.  One waiting for the name of the
/// conversation its parent was handed to is settled by the worker alone
/// (`names`), which sees the first one.
fn pair_in(s: &Shared, a: u64, b: u64, conversation_of: impl Fn(u64) -> Option<Conversation>) -> bool {
    let Some(h) = s.table().helper_of_pair(a, b).cloned() else { return false };
    if h.conv.is_none() {
        return true;
    }
    conversation_of(h.started.parent).is_some_and(|c| h.conv == Some(c.id))
}

/// The helpers waiting for the agent their parent was handed to follow
/// the conversation `now`, if that agent is in one.
pub(super) fn settle(s: &Shared, parent: u64, now: Option<&Conversation>) {
    let Some(c) = now else { return };
    let follow: Vec<u64> = s.table().under(parent).filter(|h| h.heir == Some(c.agent)).map(|h| h.sid).collect();
    for sid in follow {
        s.record(sid, Event::ConversationChanged(c.id.clone()));
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

/// [`Moved`], with the agent it was handed to.
#[derive(Debug)]
pub(super) struct Move {
    pub m: Moved,
    pub to: ProcId,
}

/// The conversation pane `sid` is in, asked before its agent is taken
/// down; None when no helper depends on it.
pub(in crate::plugins) fn leaving(sid: u64) -> Option<String> {
    let s = SHARED.get()?;
    s.table().under(sid).next()?;
    conversation_of(sid).map(|c| c.id)
}

/// The new agent of `m` is up, in front of `shell` as `is_agent` knows
/// it: noted on the main loop, written down by the worker.
pub(in crate::plugins) fn moved(m: Moved, shell: i32, is_agent: fn(&ProcRow) -> bool) {
    let Some(s) = SHARED.get() else { return };
    match crate::plugins::pty_op::front::program_in_front(shell, is_agent) {
        Some(to) => note_move(s, Move { m, to }),
        None => marspot::lx_warn!("helpers.not_moved", &format!("sid={}: the new agent left before it was looked for", m.pane)),
    }
}

/// Note move `m` with the helpers that follow it as the table says now.
pub(super) fn note_move(s: &Shared, m: Move) {
    let sids: Vec<u64> = follows(&s.table(), &m.m).map(|h| h.sid).collect();
    s.lock().moves.add(m, sids);
    s.wake();
}

/// The helpers of the conversation `m` left.
fn follows<'a>(t: &'a Table, m: &'a Moved) -> impl Iterator<Item = &'a Helper> {
    t.under(m.pane).filter(|h| h.conv.is_some() && (h.conv == m.from || h.heir == Some(m.left)))
}

/// A move not yet written down, with the helpers that follow it.
#[derive(Debug)]
pub(super) struct Pending {
    pub m: Move,
    pub sids: Vec<u64>,
}

/// The moves not yet written down, oldest first.  None is dropped: a
/// move that goes on from one still here takes that one's helpers along,
/// and a helper is in one move only, so there are no more than helpers.
#[derive(Debug, Default)]
pub(super) struct Moves(Vec<Pending>);

impl Moves {
    fn add(&mut self, m: Move, mut sids: Vec<u64>) {
        // it takes the pane from the agent that one handed it to
        for p in self.0.iter_mut().filter(|p| p.m.m.pane == m.m.pane && p.m.to == m.m.left) {
            sids.append(&mut p.sids);
        }
        sids.sort_unstable();
        sids.dedup();
        for p in &mut self.0 {
            p.sids.retain(|sid| !sids.contains(sid));
        }
        self.0.retain(|p| !p.sids.is_empty());
        if !sids.is_empty() {
            self.0.push(Pending { m, sids });
        }
    }
}

/// Write down the moves noted: the helpers of each follow the agent it
/// was handed to, or the conversation that one resumed.  The name a new
/// conversation gets is waited for from then on (`names`).
pub(super) fn write_moves(s: &Shared) {
    let pending = std::mem::take(&mut s.lock().moves.0);
    for Pending { m: Move { m, to }, sids } in pending {
        let ev = match &m.resumed {
            Some(c) => Event::ConversationChanged(c.clone()),
            None => Event::ParentAgent(to),
        };
        for sid in &sids {
            if s.table().under(m.pane).any(|h| h.sid == *sid) {
                s.record(*sid, ev.clone());
            }
        }
        if m.resumed.is_none() {
            s.lock().names.add(m.pane);
        }
    }
}

#[cfg(test)]
#[path = "tests_pair.rs"]
mod tests;
