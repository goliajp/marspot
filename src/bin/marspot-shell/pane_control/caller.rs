//! Who is asking.  The answer sets limits, the parent of a new pane and
//! the audit line; it never decides what may be done, so it has to be
//! close, not unforgeable.
//!
//! In order: a Codex thread the peer bus knows the pane of; else the
//! connecting process (`LOCAL_PEERPID`) and its ancestors, the first of
//! which that is a session's shell, matched by pid and start time; else
//! `external`, everything outside the panes, all sharing one entry.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use marspot::pidtree::ProcRow;

use super::super::observe::Observation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CallerKey {
    Pane(u64),
    External,
}

impl CallerKey {
    pub fn sid(self) -> Option<u64> {
        match self {
            CallerKey::Pane(sid) => Some(sid),
            CallerKey::External => None,
        }
    }
}

/// The connecting process, kept for the life of the connection.
#[derive(Clone, Debug, Default)]
pub(super) struct Peer {
    pub pid: Option<i32>,
    pub exe: String,
    /// The pane its ancestry leads to, worked out once.
    pub pane: Option<u64>,
}

/// The connecting process, as the kernel reports it.
pub(super) fn peer_pid(conn: &UnixStream) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pid`
    let rc = unsafe {
        libc::getsockopt(conn.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    };
    (rc == 0 && pid > 0).then_some(pid)
}

fn exe_of(pid: i32) -> String {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most `buf.len()` bytes
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

/// The first of `pid` and its ancestors that is a pane's shell.
/// `parent_of` gives a process's parent and start time; `shells` maps a
/// shell's pid to its start time and session.
pub(super) fn by_ancestry(
    pid: i32,
    parent_of: impl Fn(i32) -> Option<(i32, u64)>,
    shells: &HashMap<i32, (u64, u64)>,
) -> Option<u64> {
    let mut at = pid;
    // a process table is a tree; the bound is for one that changes under the walk
    for _ in 0..64 {
        if at <= 1 {
            return None;
        }
        let (ppid, start_us) = parent_of(at)?;
        if let Some((shell_start, sid)) = shells.get(&at)
            && *shell_start == start_us
        {
            return Some(*sid);
        }
        at = ppid;
    }
    None
}

/// `pid` as it is now, or as `obs` last saw it.
pub(super) fn row_of(pid: i32, obs: &Observation) -> Option<ProcRow> {
    marspot::pidtree::proc_row(pid).or_else(|| obs.table.get(pid).cloned())
}

/// `pid` and its ancestors up to `shell`, which is the last of them.
pub(crate) fn chain(pid: i32, row_of: impl Fn(i32) -> Option<ProcRow>, shell: i32) -> Vec<ProcRow> {
    let mut out = Vec::new();
    let mut at = pid;
    // bounded as `by_ancestry` is
    while at > 1 && out.len() < 64 {
        let Some(r) = row_of(at) else { break };
        at = if r.pid == shell { 0 } else { r.ppid };
        out.push(r);
    }
    out
}

/// Each live session's shell: pid → (start time, session).
pub(super) fn shells_of(obs: &Observation) -> HashMap<i32, (u64, u64)> {
    obs.live().filter_map(|s| s.shell.map(|id| (id.pid, (id.start_us, s.entry.id)))).collect()
}

impl Peer {
    pub fn of(conn: &UnixStream, obs: &Observation) -> Peer {
        let Some(pid) = peer_pid(conn) else { return Peer::default() };
        let shells = shells_of(obs);
        let parent_of = |p: i32| row_of(p, obs).map(|r| (r.ppid, r.start_us));
        Peer { pid: Some(pid), exe: exe_of(pid), pane: by_ancestry(pid, parent_of, &shells) }
    }
}

/// The caller of one request: the thread it named, when the peer bus
/// knows that thread's pane, else the connecting process's pane.
pub(super) fn identify(peer: &Peer, thread: Option<&str>, threads: impl FnOnce() -> Vec<(u64, String)>) -> CallerKey {
    if let Some(t) = thread
        && let Some((sid, _)) = threads().into_iter().find(|(_, th)| th == t)
    {
        return CallerKey::Pane(sid);
    }
    peer.pane.map_or(CallerKey::External, CallerKey::Pane)
}

/// The peer bus's codex threads, as `(session, thread)`.
pub(super) fn codex_threads() -> Vec<(u64, String)> {
    crate::plugins::peerbus::threads().into_iter().map(|(sid, v)| (sid, v.thread)).collect()
}
