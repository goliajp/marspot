//! A codex pane moving to another account: whether its session may go
//! now, and the mark that keeps messages out of the old home meanwhile.
//!
//! The session lives in the old home's daemon, which keeps it loaded
//! after the TUI is gone.  A message queued there in that gap runs a
//! turn nobody sees, in a home the pane has left; so from the moment a
//! move is decided until the old binding is gone, nothing is handed to
//! that daemon and senders are told so.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId};
use marspot_term::{lx_info, lx_warn};

pub(crate) use super::moving_release::RELEASE_WAIT;
use super::moving_release::Release;
use super::worker::{Binding, Event, Worker};
use crate::plugins::pty_op::Job;

/// What the codex plugin asks of the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    /// May the pane's session move?  Marked moving when it may.
    Begin { sid: u64, codex: ProcId },
    /// The move did not happen; the old process may still be there.
    End { sid: u64, codex: ProcId },
}

/// Why a session may not move now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    NotBound,
    Moving,
    Busy,
    ChildRunning,
    Queued,
    Unknown,
}

/// Finished by the worker once the old home's daemon no longer has the
/// session loaded, or with an error when it still has at the deadline.
#[derive(Clone)]
pub(crate) struct Released(pub Arc<Job>);

impl PartialEq for Released {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.0, &o.0)
    }
}

impl Eq for Released {}

impl std::fmt::Debug for Released {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Released")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Go { thread: String, home: PathBuf, released: Released },
    Held(Held),
}

/// Verdicts not yet taken; one per ask, and asks come from clicks.
const MAX_VERDICTS: usize = 64;
/// Ends not yet taken, for the same reason.
const MAX_ENDS: usize = 64;
/// How long after a move the next binding of its pane is still the
/// moved session's, owed the sentence that it may go on.
const GO_ON_WITHIN: Duration = Duration::from_secs(120);
/// A mark whose process is still there this long after it was set
/// belongs to a move that never happened: the TUI is ended within
/// seconds of the verdict.
const MARK_FOR: Duration = Duration::from_secs(120);

static ASKS: Mutex<Option<SyncSender<Event>>> = Mutex::new(None);
static VERDICTS: Mutex<Vec<(u64, ProcId, Verdict)>> = Mutex::new(Vec::new());
/// Ends wait here rather than in the worker's channel, which drops what
/// does not fit: a lost end leaves the pane refusing mail.
static ENDS: Mutex<Vec<Ask>> = Mutex::new(Vec::new());

pub(crate) fn connect(tx: Option<SyncSender<Event>>) {
    *ASKS.lock().unwrap() = tx;
}

/// Hand the worker `a`; false when there is no worker to take it.
pub(crate) fn ask(a: Ask) -> bool {
    let tx = ASKS.lock().unwrap();
    let Some(tx) = tx.as_ref() else { return false };
    if let Ask::End { .. } = a {
        let mut q = ENDS.lock().unwrap();
        if q.len() == MAX_ENDS {
            q.remove(0);
        }
        q.push(a);
        // only a wake-up: the worker takes the ends after every event
        let _ = tx.try_send(Event::MoveEnded);
        return true;
    }
    tx.try_send(Event::Move(a)).is_ok()
}

/// A stand-in for the worker, for tests on the asking side: takes
/// asks and answers none.
#[cfg(test)]
pub(crate) struct Listener(std::sync::mpsc::Receiver<Event>);

#[cfg(test)]
impl Listener {
    pub(crate) fn connect() -> Listener {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        connect(Some(tx));
        verdicts();
        ENDS.lock().unwrap().clear();
        Listener(rx)
    }

    /// The next ask, if one came.
    pub(crate) fn next(&self) -> Option<Ask> {
        while let Ok(e) = self.0.try_recv() {
            if let Event::Move(a) = e {
                return Some(a);
            }
        }
        let mut q = ENDS.lock().unwrap();
        (!q.is_empty()).then(|| q.remove(0))
    }
}

/// Every verdict reached since the last call.
pub(crate) fn verdicts() -> Vec<(u64, ProcId, Verdict)> {
    std::mem::take(&mut *VERDICTS.lock().unwrap())
}

pub(crate) fn post(sid: u64, codex: ProcId, v: Verdict) {
    let mut q = VERDICTS.lock().unwrap();
    if q.len() == MAX_VERDICTS {
        q.remove(0);
    }
    q.push((sid, codex, v));
}

pub(super) struct Mark {
    pub codex: ProcId,
    pub at: Instant,
    /// The binding was subscribed before the move unsubscribed it.
    pub resubscribe: bool,
}

/// The worker's side: which panes are moving, which are owed the go-on
/// sentence once their new process binds, and whose old home is being
/// watched for letting go of the session.
#[derive(Default)]
pub(crate) struct Moves {
    pub(super) marked: HashMap<u64, Mark>,
    go_on: HashMap<u64, (ProcId, Instant)>,
    pub(super) releases: HashMap<u64, Release>,
}

impl Moves {
    /// When the worker next has something to look at here.
    pub(super) fn due(&self) -> Option<Instant> {
        let marks = self.marked.values().map(|m| m.at + MARK_FOR);
        marks.chain(self.releases.values().map(|r| r.next)).min()
    }
}

fn alive(id: ProcId) -> bool {
    pidtree::proc_id(id.pid) == Some(id)
}

impl Worker {
    pub(super) fn on_move(&mut self, a: Ask) {
        match a {
            Ask::Begin { sid, codex } => {
                let v = self.begin_move(sid, codex);
                lx_info!("peerbus.move", &format!("{v:?}"), sid = sid);
                post(sid, codex, v);
            }
            Ask::End { sid, codex } => self.end_move(sid, codex),
        }
    }

    /// Take the ends the codex plugin left; called after every event.
    pub(super) fn take_ends(&mut self) {
        let ends = std::mem::take(&mut *ENDS.lock().unwrap());
        for a in ends {
            self.on_move(a);
        }
    }

    fn end_move(&mut self, sid: u64, codex: ProcId) {
        self.moves.go_on.remove(&sid);
        if let Some(r) = self.moves.releases.remove(&sid) {
            r.job.finish(Err("the switch ended".into()));
        }
        // a process already gone leaves its binding to the next
        // refresh, and the mark has to outlast that binding
        if self.moves.marked.get(&sid).is_some_and(|m| m.codex == codex) && alive(codex) {
            let m = self.moves.marked.remove(&sid).expect("checked above");
            self.resubscribe(sid, &m);
            lx_info!("peerbus.move.ended", "the session stays where it was", sid = sid);
        }
    }

    fn begin_move(&mut self, sid: u64, codex: ProcId) -> Verdict {
        let Some(b) = self.bindings.get(&sid).filter(|b| b.codex == codex) else {
            return Verdict::Held(Held::NotBound);
        };
        if self.moves.marked.contains_key(&sid) {
            return Verdict::Held(Held::Moving);
        }
        let (home, thread, turn, subscribed) = (b.home.clone(), b.thread.clone(), b.active_turn.is_some(), b.subscribed);
        // marked before the checks: nothing is queued between them and the move
        self.moves.marked.insert(sid, Mark { codex, at: Instant::now(), resubscribe: subscribed });
        let held = if turn { Some(Held::Busy) } else { self.why_held(&home, &thread) };
        if let Some(h) = held {
            self.moves.marked.remove(&sid);
            return Verdict::Held(h);
        }
        // the daemon unloads a session only once nobody is subscribed
        // to it, so this goes now, not when the binding does
        self.unsubscribe(&home, &thread);
        if let Some(b) = self.bindings.get_mut(&sid) {
            b.subscribed = false;
        }
        self.moves.go_on.insert(sid, (codex, Instant::now()));
        let released = self.watch_release(sid, &home, &thread);
        Verdict::Go { thread, home, released: Released(released) }
    }

    fn resubscribe(&mut self, sid: u64, m: &Mark) {
        let Some(b) = self.bindings.get(&sid).filter(|b| m.resubscribe && b.codex == m.codex && !b.subscribed) else { return };
        let (home, thread, cwd, argv) = (b.home.clone(), b.thread.clone(), b.record.cwd.clone(), b.argv.clone());
        let (class, subscribed) = self.class_for(&home, &thread, &cwd, &argv);
        let b = self.bindings.get_mut(&sid).expect("found above");
        b.subscribed = subscribed;
        b.class = class;
        b.record.class = class;
    }

    /// Clear the marks of moves that never happened and look at the
    /// old homes being watched.
    pub(super) fn moves_due(&mut self, now: Instant) {
        let late: Vec<u64> = self.moves.marked.iter().filter(|(_, m)| m.at + MARK_FOR <= now).map(|(s, _)| *s).collect();
        for sid in late {
            let codex = self.moves.marked[&sid].codex;
            let bound = self.bindings.get(&sid).is_some_and(|b| b.codex == codex);
            if alive(codex) || !bound {
                let m = self.moves.marked.remove(&sid).expect("listed above");
                lx_warn!("peerbus.move.mark_expired", "a move that never happened left its mark", sid = sid);
                self.resubscribe(sid, &m);
            } else if let Some(m) = self.moves.marked.get_mut(&sid) {
                // gone, and its binding still there: the next refresh takes both
                m.at = now;
            }
        }
        self.releases_due(now);
    }

    /// Is `sid`'s session moving?  Nothing is handed to its daemon then.
    pub(super) fn moving(&self, sid: u64) -> bool {
        self.moves.marked.contains_key(&sid)
    }

    /// The binding `b` of pane `sid` is gone.  The mark goes with it
    /// only when it is the marked process's and that process has ended:
    /// a new thread in the same TUI is not the move.
    pub(super) fn moved_out(&mut self, sid: u64, b: &Binding) {
        if self.moves.marked.get(&sid).is_some_and(|m| m.codex == b.codex) && !alive(b.codex) {
            self.moves.marked.remove(&sid);
        }
    }

    /// A new process in a pane that moved has bound: tell the session
    /// it may go on, the sentence the move took the place of.
    pub(super) fn moved_in(&mut self, sid: u64, codex: ProcId) {
        self.moves.go_on.retain(|_, (_, at)| at.elapsed() < GO_ON_WITHIN);
        if !self.moves.go_on.get(&sid).is_some_and(|(old, _)| *old != codex) {
            return;
        }
        self.moves.go_on.remove(&sid);
        let Some(b) = self.bindings.get(&sid) else { return };
        let (home, thread) = (b.home.clone(), b.thread.clone());
        let word = crate::plugins::handoff::resume_word();
        let (r, via) = self.hand_over(&home, &thread, word, None, marspot_peer::rand::uuid_v4());
        match r {
            Ok(_) => lx_info!("peerbus.move.go_on", "the moved session was told to go on", sid = sid, via = via),
            Err(e) => lx_warn!("peerbus.move.go_on_failed", &e.to_string(), sid = sid),
        }
    }
}
