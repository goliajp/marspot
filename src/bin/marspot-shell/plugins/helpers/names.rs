//! Parents whose helpers wait for the name of the new conversation the
//! pane was handed to.  The name is looked for when a conversation
//! record is written, or when an agent waited on exits; never on a
//! clock.  The first conversation the agent is seen in is the one the
//! helpers follow, so one it moves to later (a `/clear`) is not.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use marspot::pidtree::ProcId;
use marspot_term::fs_watch::Watcher;

use super::{Shared, pair};
use crate::plugins::peerbus::Conversation;

#[derive(Default)]
pub(super) struct Names {
    /// The parents waiting.
    pub unnamed: HashSet<u64>,
    /// Something they wait on changed, or one was added: look.
    due: bool,
    watch: Option<Watch>,
    /// The record directories watched; None: the registry's.
    pub dirs: Option<Vec<PathBuf>>,
}

struct Watch {
    files: Vec<PathBuf>,
    pids: Vec<i32>,
    w: Watcher,
}

impl Names {
    /// `parent` waits from now on.
    pub fn add(&mut self, parent: u64) {
        self.unnamed.insert(parent);
        self.due = true;
    }

    #[cfg(test)]
    pub fn watching(&self) -> bool {
        self.watch.is_some()
    }

    /// Wait for nothing: the app was put away, or started again.
    pub fn clear(&mut self) {
        self.unnamed.clear();
        self.due = false;
        self.watch = None;
    }
}

/// The agents the waiting parents were handed to.
fn heirs(s: &Shared) -> Vec<ProcId> {
    let parents: Vec<u64> = s.lock().names.unnamed.iter().copied().collect();
    let t = s.table();
    let mut out: Vec<ProcId> = parents.iter().flat_map(|p| t.under(*p).filter_map(|h| h.heir)).collect();
    out.sort_unstable_by_key(|p| (p.pid, p.start_us));
    out.dedup();
    out
}

/// Watch the record directories, each heir's own record (claude
/// rewrites it in place) and each heir's exit, while a parent waits;
/// stop once none does.
fn sync_watch(s: &Arc<Shared>) {
    let heirs = heirs(s);
    if heirs.is_empty() {
        s.lock().names.watch = None;
        return;
    }
    let dirs = s.lock().names.dirs.clone().unwrap_or_else(marspot_peer::registry::default_dirs);
    let mut files = dirs.clone();
    files.extend(dirs.iter().flat_map(|d| heirs.iter().map(move |h| d.join(format!("{}.json", h.pid)))));
    let pids: Vec<i32> = heirs.iter().map(|h| h.pid).collect();
    let same = |w: &Watch| w.files == files && w.pids == pids && w.w.alive();
    if s.lock().names.watch.as_ref().is_some_and(same) {
        return;
    }
    let weak = Arc::downgrade(s);
    let spawned = Watcher::spawn(&files, move || {
        if let Some(s) = weak.upgrade() {
            s.lock().names.due = true;
            s.wake();
        }
    });
    let w = match spawned {
        Ok(w) => w,
        Err(e) => {
            marspot::lx_warn!("helpers.names_unwatched", &format!("no conversation name is waited for: {e}"));
            s.lock().names.watch = None;
            return;
        }
    };
    // one that exited already is not waited on: the look below sees it
    let gone = heirs.iter().any(|h| marspot::pidtree::proc_id(h.pid) != Some(*h) || w.watch_pid(h.pid).is_err());
    let mut st = s.lock();
    st.names.watch = Some(Watch { files, pids, w });
    st.names.due |= gone;
}

/// On the worker, after a job: if something the waiting parents wait on
/// changed, look at them.
pub(super) fn on_change(s: &Arc<Shared>, conversation_of: impl Fn(u64) -> Option<Conversation>) {
    if !std::mem::take(&mut s.lock().names.due) {
        return;
    }
    // armed before the look: a record written after it is an event
    sync_watch(s);
    look(s, &|p: ProcId| marspot::pidtree::proc_id(p.pid) == Some(p), conversation_of);
    sync_watch(s);
}

/// Look at the waiting parents: the helpers of each follow the
/// conversation the agent it was handed to is in now, if it is in one.
/// A parent stops waiting once that agent is gone (`alive`): its
/// helpers follow no conversation, until the pane is handed on again.
pub(super) fn look(s: &Shared, alive: &dyn Fn(ProcId) -> bool, conversation_of: impl Fn(u64) -> Option<Conversation>) {
    let parents: Vec<u64> = s.lock().names.unnamed.iter().copied().collect();
    for parent in parents {
        let waits = |p: &Option<ProcId>| p.is_some_and(alive);
        let heirs = s.table().under(parent).filter(|h| h.heir.is_some()).count();
        if s.table().under(parent).any(|h| waits(&h.heir)) {
            pair::settle(s, parent, conversation_of(parent).as_ref());
            if s.table().under(parent).any(|h| waits(&h.heir)) {
                continue;
            }
        } else if heirs > 0 {
            marspot::lx_warn!("helpers.heir_gone", &format!("parent={parent}: the agent it was handed to left before it named a conversation; its helpers follow none"));
        }
        s.lock().names.unnamed.remove(&parent);
    }
}

/// At a start, helpers that waited for a name: whether the conversation
/// their parent shows now is the first one there is not known (it may
/// be one after a `/clear`), so they follow none.
pub(super) fn at_start(s: &Shared) {
    s.lock().names.clear();
    let t = s.table();
    for h in t.map.values().filter(|h| h.heir.is_some()) {
        let why = "its parent was handed to a new conversation not named before this start; it follows none";
        marspot::lx_warn!("helpers.unnamed_at_start", &format!("sid={} parent={}: {why}", h.sid, h.started.parent));
    }
}

#[cfg(test)]
#[path = "tests_names.rs"]
mod tests;
