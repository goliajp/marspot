//! The gemini panes, as each plugin tick sees them, handed to the
//! allocation, and its moves made when `gemini.allocation` is `live`.
//!
//! Off (the default) gathers and reads none of it.  The screen is read
//! for what the session file does not record: gemini's quota dialog,
//! a tool approval and typed text.  Once per quiet stretch: when the
//! session file has gone twenty seconds without a write, and again only
//! after it has been written, unless gemini still looked busy then: a
//! quota failure can come after retries that write nothing, so a busy
//! screen is read again after [`REREAD`] and then left.  A live move
//! reads it once more just before it is made.  A move never answers the dialog: the gemini
//! behind it is stopped, so no paid choice is ever made.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use marspot::settings::AgentAllocation;

use super::alloc::{GeminiAlloc, Move, PaneIn, Snapshot, events_dir};
use super::pane::{config_dir, handoff_homes, profile_switch_op};
use super::quota::{How, Refusal, Scope, from_screen};
use super::screen::{asks_approval, has_typed, is_busy};
use super::{GeminiPlugin, is_gemini, session_by_id};
use crate::observe::{Observation, Session};
use crate::plugins::claudecode::with_helpers::HelperRoles;
use crate::plugins::pidtree::ProcId;
use crate::plugins::{LogLevel, PluginHost, handoff};

/// A session file this long without a write: no turn is running.
const QUIET: Duration = Duration::from_secs(20);

/// A gemini that still works (it fell back to another model) is moved
/// only once its session file has been quiet this long and the user is
/// in another pane, as a resting claude is.
const AWAY: Duration = Duration::from_secs(60);

/// A child the worker starts this long after itself is a tool's run;
/// the ones it starts with are its MCP servers, which live as long as
/// it does.
const TOOL_AFTER_US: u64 = 10_000_000;

/// The gaps between reads of a screen that still showed gemini busy,
/// within one quiet stretch; after the last, it is not read again.
const REREAD: [i64; 3] = [20, 60, 300];

/// A pane is moved at most this often.
const MOVE_GAP: Duration = Duration::from_secs(600);

/// One gemini pane's facts besides what the badge menu keeps.
pub(super) struct AllocSeen {
    pub sid: u64,
    pub started_at: i64,
    pub refused: Option<Refusal>,
    /// When its session file was last written.
    pub written: Option<SystemTime>,
    /// Its gemini has a tool's process running.
    pub tool_running: bool,
    pub helper: bool,
}

/// The tick's gathering of [`AllocSeen`]: nothing at all when off.
pub(super) struct Gather {
    on: Option<(HelperRoles, Arc<Observation>)>,
    pub(super) seen: Vec<AllocSeen>,
}

impl Gather {
    pub(super) fn new(mode: AgentAllocation, obs: &Arc<Observation>) -> Gather {
        let on = (mode != AgentAllocation::Off).then(|| (HelperRoles::of_observation(obs), obs.clone()));
        Gather { on, seen: Vec::new() }
    }

    pub(super) fn push(&mut self, sid: u64, gemini: ProcId, worker: ProcId, refused: Option<&Refusal>, written: Option<SystemTime>) {
        let Some((roles, obs)) = &self.on else { return };
        let tool_running = obs.sessions.iter().find(|s| s.entry.id == sid).is_some_and(|s| tool_running(s, worker));
        let (started_at, helper) = ((gemini.start_us / 1_000_000) as i64, roles.is_helper(sid));
        self.seen.push(AllocSeen { sid, started_at, refused: refused.cloned(), written, tool_running, helper });
    }
}

/// Does gemini's worker have a tool's process under it?
pub(super) fn tool_running(s: &Session, worker: ProcId) -> bool {
    s.descendants.iter().any(|p| p.row.ppid == worker.pid && !is_gemini(p) && p.row.start_us > worker.start_us + TOOL_AFTER_US)
}

/// One read of a pane's screen, good while its session file stays as
/// it was then.
#[derive(Clone)]
struct Look {
    written: Option<SystemTime>,
    at: i64,
    dialog: Option<Refusal>,
    held: Option<&'static str>,
    /// gemini still worked at a turn: no dialog, nothing that holds it.
    busy: bool,
    /// How often the screen has been read in this quiet stretch.
    reads: u8,
}

pub(super) struct AllocPass {
    core: Option<GeminiAlloc>,
    pub(super) dir: Option<PathBuf>,
    looks: HashMap<u64, Look>,
    /// When each pane's quota dialog was first seen, while it stays up.
    dialog_since: HashMap<u64, (Scope, i64)>,
    moved_at: HashMap<u64, Instant>,
    pub(super) focused: Option<u64>,
    pub(super) screen: fn(u64) -> Option<String>,
}

impl Default for AllocPass {
    fn default() -> AllocPass {
        let screen: fn(u64) -> Option<String> = |sid| marspot::pane_read::screen(sid, 0).ok();
        AllocPass { core: None, dir: None, looks: HashMap::new(), dialog_since: HashMap::new(), moved_at: HashMap::new(), focused: None, screen }
    }
}

impl AllocPass {
    fn read(&mut self, sid: u64, written: Option<SystemTime>, now: i64) -> Look {
        let got = (self.screen)(sid);
        let screen = got.as_deref().unwrap_or_default();
        // timed from when it was first seen, so a "reset after" in it
        // does not move with every read
        let dialog = match from_screen(screen, now) {
            Some(d) => {
                let kept = self.dialog_since.get(&sid).filter(|(scope, _)| *scope == d.scope).map(|(_, t)| *t);
                if kept.is_none() {
                    self.dialog_since.insert(sid, (d.scope, now));
                }
                from_screen(screen, kept.unwrap_or(now))
            }
            None => {
                self.dialog_since.remove(&sid);
                None
            }
        };
        let held = if asks_approval(screen) {
            Some("gemini asks to approve a tool")
        } else if has_typed(screen) {
            Some("text is typed in its input box")
        } else {
            None
        };
        let busy = got.is_some() && dialog.is_none() && held.is_none() && is_busy(screen);
        let reads = self.looks.get(&sid).filter(|l| l.written == written).map_or(1, |l| l.reads.saturating_add(1));
        let look = Look { written, at: now, dialog, held, busy, reads };
        self.looks.insert(sid, look.clone());
        look
    }

    /// The pane's screen as last read in this quiet stretch, read now
    /// when the stretch is old enough and has not been read, or when the
    /// last read showed gemini busy and its gap has passed.
    fn look(&mut self, s: &AllocSeen, now: i64) -> Option<Look> {
        if self.looks.get(&s.sid).is_some_and(|l| l.written != s.written) {
            self.looks.remove(&s.sid);
        }
        if let Some(l) = self.looks.get(&s.sid) {
            let gap = REREAD.get(usize::from(l.reads) - 1);
            if !(l.busy && gap.is_some_and(|g| now - l.at >= *g)) {
                return Some(l.clone());
            }
            return Some(self.read(s.sid, s.written, now));
        }
        quiet_for(s).is_some_and(|q| q >= QUIET).then(|| self.read(s.sid, s.written, now))
    }

    /// Why pane `s` is to stay though quiet, if it is.
    fn held(&self, s: &AllocSeen, look: Option<&Look>) -> Option<&'static str> {
        if s.tool_running {
            return Some("a tool of its gemini is running");
        }
        if let Some(h) = look.and_then(|l| l.held) {
            return Some(h);
        }
        // a gemini that met no dialog and no stopped turn goes on working
        let stopped = look.is_some_and(|l| l.dialog.is_some()) || s.refused.as_ref().is_some_and(|r| r.how == How::Error);
        // no focus event since the shell started: the user may be anywhere
        let away = self.focused.is_some_and(|f| f != s.sid) && quiet_for(s).is_some_and(|q| q >= AWAY);
        (!stopped && !away).then_some("its gemini still works and the user may be at it")
    }
}

fn quiet_for(s: &AllocSeen) -> Option<Duration> {
    s.written.and_then(|w| w.elapsed().ok())
}

/// Does home `dir` hold session `session` of the project at `cwd`?
fn holds(dir: &Path, cwd: &str, session: &str) -> bool {
    session_by_id(&config_dir(dir), cwd, session).is_some()
}

impl GeminiPlugin {
    /// One pass of the allocation over the gemini panes.
    pub(super) fn allocate(&mut self, host: &dyn PluginHost, mode: AgentAllocation, seen: Vec<AllocSeen>, now: i64) {
        if mode == AgentAllocation::Off {
            return;
        }
        let homes = handoff_homes();
        let live: Vec<u64> = seen.iter().map(|s| s.sid).collect();
        self.alloc.looks.retain(|sid, _| live.contains(sid));
        self.alloc.dialog_since.retain(|sid, _| live.contains(sid));
        self.alloc.moved_at.retain(|sid, t| live.contains(sid) && t.elapsed() < MOVE_GAP);
        let mut panes = Vec::new();
        let mut written = HashMap::new();
        for s in seen {
            let Some(pane) = self.panes.get(&s.sid) else { continue };
            let look = self.alloc.look(&s, now);
            let held = self.alloc.held(&s, look.as_ref());
            written.insert(s.sid, s.written);
            let (profile, model) = (pane.profile.unwrap_or(0), pane.model.clone());
            let quiet = quiet_for(&s).is_some_and(|q| q >= QUIET);
            let on_screen = look.and_then(|l| l.dialog);
            panes.push(PaneIn { sid: s.sid, profile, started_at: s.started_at, model, refused: s.refused, on_screen, quiet, held, helper: s.helper });
        }
        let snap = Snapshot { now, homes: homes.iter().map(|(n, _)| *n).collect(), panes };
        let dir = self.alloc.dir.clone().unwrap_or_else(|| events_dir(&marspot::paths::state_root()));
        let core = self.alloc.core.get_or_insert_with(|| GeminiAlloc::new(dir));
        let panes = &self.panes;
        let (mut log, moves) = core.observe(&snap, |p, to| {
            let pane = panes.get(&p.sid);
            let at = homes.iter().find(|(n, _)| *n == to);
            match (pane.and_then(|x| x.cwd.as_deref()), pane.and_then(|x| x.session.as_deref()), at) {
                (Some(cwd), Some(session), Some((_, dir))) => holds(dir, cwd, session),
                _ => false,
            }
        });
        super::alloc_helper::publish(core.refusals(), snap.panes.iter().map(|p| p.profile).collect());
        if mode == AgentAllocation::Live {
            for m in moves {
                if self.alloc.moved_at.contains_key(&m.sid) || !self.still_clear(m.sid, written.get(&m.sid).copied().flatten(), now) {
                    continue;
                }
                if self.make_move(host, &m, &homes) {
                    self.alloc.moved_at.insert(m.sid, Instant::now());
                    if let Some(core) = self.alloc.core.as_mut() {
                        core.moved(now, &m, &mut log);
                    }
                }
            }
        }
        for (tag, line) in log {
            host.log(LogLevel::Info, tag, &line);
        }
    }

    /// The screen read in an earlier pass may be out of date: read it
    /// again, once, right before a move.  A read that holds the pane
    /// stands until its session file is written again.
    fn still_clear(&mut self, sid: u64, written: Option<SystemTime>, now: i64) -> bool {
        match self.alloc.looks.get(&sid) {
            Some(l) if l.at == now => l.held.is_none(),
            _ => self.alloc.read(sid, written, now).held.is_none(),
        }
    }

    /// Resume the session in the target home, or hand it over there.
    fn make_move(&mut self, host: &dyn PluginHost, m: &Move, homes: &[(u8, PathBuf)]) -> bool {
        let (Some(pane), Some((_, dir))) = (self.panes.get(&m.sid).cloned(), homes.iter().find(|(n, _)| *n == m.to)) else { return false };
        host.log(LogLevel::Info, "gemini.alloc.move", &format!("sid={} P{} → P{} ({})", m.sid, m.from, m.to, if m.resume { "resume" } else { "handoff" }));
        if !m.resume {
            return self.hand_off(host, m.sid, &pane, handoff::Agent::Gemini, m.to);
        }
        let (Some(session), Some(home)) = (pane.session.as_deref(), std::env::var_os("HOME").map(PathBuf::from)) else { return false };
        let Some(op) = profile_switch_op(&pane, m.to, dir, session, &home) else { return false };
        match host.submit_pty_op(m.sid, op) {
            Ok(()) => true,
            Err(e) => {
                host.log(LogLevel::Warn, "gemini.alloc.submit_failed", &format!("sid={}: {e}", m.sid));
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "alloc_wire_tests.rs"]
mod tests;
