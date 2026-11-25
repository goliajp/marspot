//! Which pane a process belongs to.
//!
//! A process under a pane's shell is that pane's.  One outside every
//! pane (adopted by launchd, or run by a codex app-server for a thread)
//! is placed by its environment, the first source that names a pane
//! winning, in order: the codex thread it runs for, the claude session
//! that started it (or the one that session's conversation went on
//! in), the pane token it inherited.  A source that names no pane is
//! passed over rather than taken as an answer: a claude pane running
//! `codex exec` hands its children a thread no pane is bound to, next
//! to its own session id, and a codex pane running `claude -p` hands
//! them a session id no pane holds, next to its own thread.  The
//! descendants of such a process go where it goes.  A process a codex
//! app-server runs with no thread (an MCP server) serves every thread
//! of the server, so the token it inherited from the server is not
//! taken for it either.
//!
//! This decides whom to tell and who may end a process.  Any process of
//! the user may signal any other, so it keeps a session from hitting
//! another pane's work by mistake; it is not a security boundary.

use std::collections::HashMap;

use marspot::pidtree::{ProcId, ProcRow};

use crate::observe::{ArgvCache, EnvCache, Observation};
use crate::plugins::claudecode::{ClaudePane, pane_of_session};

/// Processes outside the panes whose environment is read for the first
/// time, at most, in one check; the rest wait for the next.  One read
/// takes the whole environment of one process.
pub(crate) const NEW_PER_CHECK: usize = 64;

/// The environment keys a placement reads.
const KEYS: [&str; 3] = ["CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "MARSPOT_SUBMIT_TOKEN"];

/// The bound on an ancestor walk, for a table that changed under it.
const DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// A live pane.
    Pane(u64),
    /// A pane that is closed: the token it inherited names a session
    /// that is no longer running.
    Closed(u64),
    /// Nothing names a pane, or the environment cannot be read.
    Unknown,
    /// Not decided in this check: its environment is past this check's
    /// reads.
    Later,
}

/// What each agent identity is bound to, as of one check.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct Known {
    /// Codex thread → pane.
    pub codex_threads: HashMap<String, u64>,
    pub claude: Vec<ClaudePane>,
}

impl Known {
    pub fn current() -> Known {
        let codex_threads = crate::plugins::peerbus::threads().into_iter().map(|(sid, v)| (v.thread, sid)).collect();
        Known { codex_threads, claude: crate::plugins::claudecode::sessions() }
    }
}

/// Places processes, over the checks of the watch.  The caches live as
/// long as the processes they are about.
#[derive(Default)]
pub(crate) struct Attributor {
    env: EnvCache,
    argv: ArgvCache,
    /// pid → (start time, pane) for every live pane's shell and
    /// descendants, as of this check.
    in_pane: HashMap<i32, (u64, u64)>,
    /// The bindings and live panes the placements in `roots` were made
    /// against.
    known: Known,
    live: Vec<u64>,
    /// Outside processes placed, kept across checks while `known` and
    /// `live` stay the same: placing one can cost a stat per claude
    /// pane.
    roots: HashMap<ProcId, Owner>,
    /// Outside processes whose environment was read in this check.
    new: usize,
    /// Outside processes placed in this check rather than taken from
    /// `roots`.
    placed: usize,
}

impl Attributor {
    /// Start a check against `obs`, with the bindings `known`.
    pub fn begin(&mut self, obs: &Observation, known: &Known) {
        self.env.prune(obs);
        self.argv.prune(&obs.table);
        let live: Vec<u64> = obs.live().map(|s| s.entry.id).collect();
        // the panes come out of a map in no fixed order
        let mut known = known.clone();
        known.claude.sort_by_key(|p| p.sid);
        if known != self.known || live != self.live {
            self.known = known;
            self.live = live;
            self.roots.clear();
        } else {
            self.roots.retain(|id, _| obs.has(*id));
        }
        self.new = 0;
        self.placed = 0;
        self.in_pane.clear();
        for s in obs.live() {
            let Some(shell) = s.shell else { continue };
            self.in_pane.insert(shell.pid, (shell.start_us, s.entry.id));
            for p in &s.descendants {
                self.in_pane.insert(p.row.pid, (p.row.start_us, s.entry.id));
            }
        }
    }

    /// The pane `row` belongs to.  `begin` must have been called with
    /// the same `obs`.
    pub fn owner(&mut self, obs: &Observation, row: &ProcRow) -> Owner {
        if let Some(sid) = self.pane_of(row) {
            return Owner::Pane(sid);
        }
        let Some((root, of_server)) = self.outside_root(obs, row) else { return Owner::Unknown };
        if let Some(o) = self.roots.get(&root.id()) {
            return *o;
        }
        // only a read the cache cannot answer spends the budget
        if !self.env.has_all(&root, &KEYS) {
            if self.new >= NEW_PER_CHECK {
                return Owner::Later;
            }
            self.new += 1;
            if !self.env.load(&root, &KEYS) {
                self.roots.insert(root.id(), Owner::Unknown);
                return Owner::Unknown;
            }
        }
        self.placed += 1;
        let o = self.by_env(obs, &root, of_server);
        self.roots.insert(root.id(), o);
        o
    }

    /// The claude session an outside process inherited, from what
    /// placing it read; None before it was placed or when it has none.
    pub fn session(&mut self, obs: &Observation, row: &ProcRow) -> Option<String> {
        let (root, _) = self.outside_root(obs, row)?;
        if !self.env.has_all(&root, &KEYS) {
            return None;
        }
        self.env.value(&root, KEYS[1])
    }

    /// Outside processes placed in this check rather than remembered
    /// from an earlier one.
    #[cfg(test)]
    pub fn placed(&self) -> usize {
        self.placed
    }

    fn pane_of(&self, row: &ProcRow) -> Option<u64> {
        self.in_pane.get(&row.pid).filter(|(start, _)| *start == row.start_us).map(|(_, sid)| *sid)
    }

    /// The process `row`'s placement follows: the first of it and its
    /// ancestors whose parent is launchd or a codex app-server, and
    /// whether that parent is the app-server.  None for a process under
    /// anything else that is not a pane: a desktop app's, a system
    /// daemon's.
    fn outside_root(&mut self, obs: &Observation, row: &ProcRow) -> Option<(ProcRow, bool)> {
        let mut at = row.clone();
        for _ in 0..DEPTH {
            if at.ppid == 1 {
                return Some((at, false));
            }
            let parent = obs.table.get(at.ppid)?;
            if self.pane_of(parent).is_some() {
                return None;
            }
            if self.is_app_server(parent) {
                return Some((at, true));
            }
            at = parent.clone();
        }
        None
    }

    fn is_app_server(&mut self, row: &ProcRow) -> bool {
        if row.comm != "codex" {
            return false;
        }
        self.argv.get(row).is_some_and(|a| a.get(1).is_some_and(|s| s == "app-server"))
    }

    /// `root`'s owner by its environment, which is in the cache.
    fn by_env(&mut self, obs: &Observation, root: &ProcRow, of_server: bool) -> Owner {
        let thread = self.env.value(root, KEYS[0]);
        if let Some(sid) = thread.as_ref().and_then(|t| self.known.codex_threads.get(t).copied()) {
            return Owner::Pane(sid);
        }
        if let Some(sid) = self.env.value(root, KEYS[1]).and_then(|s| pane_of_session(&self.known.claude, &s)) {
            return Owner::Pane(sid);
        }
        if of_server && thread.is_none() {
            return Owner::Unknown;
        }
        let Some(token) = self.env.value(root, KEYS[2]) else { return Owner::Unknown };
        match token_sid(&token) {
            Some(sid) if obs.session(sid).is_some_and(|s| s.live) => Owner::Pane(sid),
            Some(sid) => Owner::Closed(sid),
            None => Owner::Unknown,
        }
    }
}

/// The pane a token names, when it is that pane's: `<sid hex>-<check>`.
fn token_sid(token: &str) -> Option<u64> {
    let (hex, _) = token.split_once('-')?;
    let sid = u64::from_str_radix(hex, 16).ok()?;
    marspot_term::submit_receipt::token_names(sid, token).then_some(sid)
}

#[cfg(test)]
mod tests;
