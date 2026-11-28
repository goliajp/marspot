//! A shadow pane: a session with no window that belongs to another
//! session, its parent, and ends with it.
//!
//! Whether this session is one, and whose, is the registry entry's
//! `shadow_of`, which this process writes from the first entry on.  The
//! core names the parent when it starts the session (`MARSPOT_SHADOW_OF`,
//! read once and removed: a variable left in the environment would
//! outlive an execv and make a pane moved into a window a shadow again),
//! an image swap carries it in the manifest, and `SessionShadow` changes
//! it.
//!
//! A shadow ends on its own, with no core or app needed: when its
//! parent's process exits, whose pid is one more thing the session's
//! file watcher waits on, and when it has neither printed nor been typed
//! into for its idle timeout, a deadline the main loop sleeps toward.
//! A child the user made has no idle timeout (`SHADOW_IDLE_NEVER`).
//!
//! Panes nest two levels at most: a parent is never itself a child, and
//! a pane with children never becomes one.  L1 holds every change to
//! that; a session refuses one that breaks it all the same, and one that
//! starts under a child (an image from before the rule, or a spawn that
//! raced a move) is put under that child's parent instead.
//! Either way it ends as when its shell exits: the shell is hung up, its
//! regions and its directory go.

use std::time::{Duration, Instant};

use marspot_term::fs_watch::Watcher;
use marspot_term::shell_proto::{
    DoneCode, Frame, MsgType, PaneDone, SHADOW_IDLE_DEFAULT_S, SHADOW_IDLE_NEVER, SessionShadow, encode_pane_done,
    shadow_was_text,
};
use marspot_term::{lx_event, lx_warn, platform, session_registry};

use crate::control_writer::ControlWriter;

pub(crate) const ENV_SHADOW_OF: &str = "MARSPOT_SHADOW_OF";
pub(crate) const ENV_SHADOW_IDLE_S: &str = "MARSPOT_SHADOW_IDLE_S";
/// How many parents up a chain is followed: the registry is written by
/// the sessions, so a broken one can loop.
const WALK_MAX: usize = 16;

/// Whose shadow this session is (`parent` 0: nobody's) and how long it
/// may sit idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShadowCfg {
    pub(crate) parent: u64,
    pub(crate) idle_s: u32,
}

impl ShadowCfg {
    /// A timeout of 0 is the default one and `SHADOW_IDLE_NEVER` none;
    /// a pane that is no shadow has none either.
    pub(crate) fn new(parent: u64, idle_s: u32) -> Self {
        match (parent, idle_s) {
            (0, _) => Self::default(),
            (parent, 0) => Self { parent, idle_s: SHADOW_IDLE_DEFAULT_S },
            (parent, idle_s) => Self { parent, idle_s },
        }
    }
}

/// What the core asked for in the environment, which leaves it here:
/// neither the shell nor a later image may see it.  A parent named with
/// no timeout, or 0, gets the default one.
pub(crate) fn take_env() -> ShadowCfg {
    let read = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
    let cfg = ShadowCfg::new(read(ENV_SHADOW_OF), u32::try_from(read(ENV_SHADOW_IDLE_S)).unwrap_or(SHADOW_IDLE_NEVER));
    // SAFETY: first thing in main, before any other thread or the shell starts
    unsafe {
        std::env::remove_var(ENV_SHADOW_OF);
        std::env::remove_var(ENV_SHADOW_IDLE_S);
    }
    cfg
}

pub(crate) struct Shadow {
    sid: u64,
    cfg: ShadowCfg,
    /// The parent's process, as confirmed: pid and start time.
    parent: Option<(i32, u64)>,
    /// The parent could not be confirmed, or its process went.
    parent_gone: bool,
    /// The output position and input count last seen, and when either moved.
    seen: (u64, u64),
    active_at: Instant,
}

impl Shadow {
    pub(crate) fn new(sid: u64, cfg: ShadowCfg, watcher: Option<&Watcher>) -> Self {
        let mut s = Self {
            sid,
            cfg,
            parent: None,
            parent_gone: false,
            seen: (0, crate::input::count()),
            active_at: Instant::now(),
        };
        s.follow(watcher);
        s
    }

    pub(crate) fn cfg(&self) -> ShadowCfg {
        self.cfg
    }

    /// Confirm the parent's process and ask `watcher` to report its exit.
    fn follow(&mut self, watcher: Option<&Watcher>) {
        self.parent = None;
        self.parent_gone = false;
        if self.cfg.parent == 0 {
            return;
        }
        match confirm(self.cfg.parent) {
            // looked at again once watched: a parent gone in between may
            // never report its exit
            Some((pid, start)) if watcher.is_none_or(|w| w.watch_pid(pid).is_ok()) && alive(pid, start) => {
                self.parent = Some((pid, start));
                lx_event!("L3_SHADOW_FOLLOWS", "a shadow pane, ending with its parent", session_id = self.sid,
                    parent = self.cfg.parent, parent_pid = pid, idle_s = self.cfg.idle_s);
            }
            _ => self.parent_gone = true,
        }
    }

    /// When the idle timeout runs out, for the main loop's sleep.
    pub(crate) fn due_in(&self) -> Option<Duration> {
        self.times_out().then(|| (self.active_at + self.idle()).saturating_duration_since(Instant::now()))
    }

    fn times_out(&self) -> bool {
        self.cfg.parent != 0 && self.cfg.idle_s != SHADOW_IDLE_NEVER
    }

    fn idle(&self) -> Duration {
        Duration::from_secs(u64::from(self.cfg.idle_s))
    }

    /// Output or input since the last call restarts the idle timeout.
    pub(crate) fn note(&mut self, pos: u64) {
        if self.cfg.parent == 0 {
            return;
        }
        let seen = (pos, crate::input::count());
        if seen != self.seen {
            self.seen = seen;
            self.active_at = Instant::now();
        }
    }

    /// Why this session has to end now, if it does.  `pos` is the output
    /// position; `looked` says something the watcher reports changed, so
    /// the parent is looked at again.
    pub(crate) fn ended(&mut self, pos: u64, looked: bool) -> Option<&'static str> {
        if self.cfg.parent == 0 {
            return None;
        }
        self.note(pos);
        if looked && !self.parent_gone {
            self.parent_gone = !self.parent.is_some_and(|(pid, start)| alive(pid, start));
        }
        if self.parent_gone {
            Some("the parent session is gone")
        } else if self.times_out() && self.active_at.elapsed() >= self.idle() {
            Some("idle for the shadow timeout")
        } else {
            None
        }
    }

    /// `SessionShadow`: become the shadow of another session, or of none,
    /// in memory and in the registry entry, then answer.
    pub(crate) fn set(&mut self, m: SessionShadow, watcher: Option<&Watcher>, poke: Option<&mut ControlWriter>) {
        let cfg = ShadowCfg::new(m.parent_sid, m.idle_s);
        let answer = if cfg.parent == self.sid || (cfg.parent != 0 && confirm(cfg.parent).is_none()) {
            (DoneCode::NoSuchSession, "no such session".to_string())
        } else if let Some(why) = (cfg.parent != 0).then(|| third_level(self.sid, cfg.parent)).flatten() {
            lx_warn!("l3.shadow.third_level_refused", why, session_id = self.sid, parent = cfg.parent);
            (DoneCode::Other, why.to_string())
        } else {
            match write_entry(self.sid, cfg.parent) {
                Ok(()) => {
                    let was = self.cfg.idle_s;
                    self.cfg = cfg;
                    self.active_at = Instant::now();
                    self.follow(watcher);
                    lx_event!("L3_SHADOW_SET", "shadow parent changed on request", session_id = self.sid,
                        parent = cfg.parent, idle_s = cfg.idle_s);
                    (DoneCode::Ok, shadow_was_text(was))
                }
                Err(e) => {
                    lx_warn!("l3.shadow.entry_write_failed", &format!("{e}"), session_id = self.sid);
                    (DoneCode::Other, format!("entry: {e}"))
                }
            }
        };
        let done = PaneDone { req_id: m.req_id, code: answer.0, text: answer.1 };
        if let (Some(w), Ok(p)) = (poke, encode_pane_done(&done)) {
            let _ = w.send(Frame::new(MsgType::PaneDone, p));
        }
    }
}

/// Why making `sid` a child of `parent` would nest panes three levels
/// deep, if it would.
fn third_level(sid: u64, parent: u64) -> Option<&'static str> {
    if session_registry::read_session_entry(parent).is_ok_and(|e| e.shadow_of != 0) {
        return Some(THIRD_LEVEL_PARENT);
    }
    session_registry::list_session_entries()
        .iter()
        .any(|e| e.shadow_of == sid && e.id != sid && session_registry::entry_is_live(e))
        .then_some(THIRD_LEVEL_CHILDREN)
}

pub(crate) const THIRD_LEVEL_PARENT: &str = "the parent is itself a child pane";
pub(crate) const THIRD_LEVEL_CHILDREN: &str = "a pane with child panes cannot become a child pane";

/// `cfg` with its parent moved up to the top of the parent's chain
/// when the parent is itself a child, said in the log.  The registry
/// entry is not written here: this runs before the session writes it.
pub(crate) fn flattened(sid: u64, cfg: ShadowCfg) -> ShadowCfg {
    let parent_of = |s: u64| session_registry::read_session_entry(s).map_or(0, |e| e.shadow_of);
    let mut top = cfg.parent;
    for _ in 0..WALK_MAX {
        match parent_of(top) {
            0 => break,
            p if p == sid => break,
            p => top = p,
        }
    }
    if top == cfg.parent {
        return cfg;
    }
    lx_event!("L3_SHADOW_FLATTENED", "the parent was itself a child pane; now under the pane at the top",
        session_id = sid, was = cfg.parent, parent = top);
    ShadowCfg { parent: top, ..cfg }
}

fn write_entry(sid: u64, parent: u64) -> std::io::Result<()> {
    let mut entry = session_registry::read_session_entry(sid)?;
    entry.shadow_of = parent;
    session_registry::rewrite_session_entry(&entry)
}

/// The live L3 of session `sid`: its pid and start time.
fn confirm(sid: u64) -> Option<(i32, u64)> {
    let entry = session_registry::read_session_entry(sid).ok()?;
    if !session_registry::entry_is_live(&entry) {
        return None;
    }
    Some((entry.pid, platform::pid_start_us(entry.pid)?))
}

/// Still the process that was confirmed.  A process that has exited and
/// not been collected has no executable to name, so it is not alive.
fn alive(pid: i32, start: u64) -> bool {
    platform::pid_start_us(pid) == Some(start) && platform::pid_exe_name(pid).is_some_and(|n| n == "marspot-session")
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::test_env::EnvGuard;

    /// The variables are read once and leave the environment, so neither
    /// the shell nor a later image sees them; a missing timeout is the
    /// default one, and a pane named nobody's has none.
    #[test]
    fn shadow_session_env_is_taken_once() {
        let mut env = EnvGuard::new();
        env.set(ENV_SHADOW_OF, "42");
        env.remove(ENV_SHADOW_IDLE_S);
        assert_eq!(take_env(), ShadowCfg { parent: 42, idle_s: SHADOW_IDLE_DEFAULT_S });
        assert!(std::env::var_os(ENV_SHADOW_OF).is_none());
        assert_eq!(take_env(), ShadowCfg::default(), "gone after the first read");
        env.set(ENV_SHADOW_OF, "42");
        env.set(ENV_SHADOW_IDLE_S, "2");
        assert_eq!(take_env(), ShadowCfg { parent: 42, idle_s: 2 });
        assert!(std::env::var_os(ENV_SHADOW_IDLE_S).is_none());
        env.set(ENV_SHADOW_OF, "42");
        env.set(ENV_SHADOW_IDLE_S, "0");
        assert_eq!(take_env(), ShadowCfg { parent: 42, idle_s: SHADOW_IDLE_DEFAULT_S }, "0 is the default, as before");
        env.set(ENV_SHADOW_OF, "42");
        env.set(ENV_SHADOW_IDLE_S, &SHADOW_IDLE_NEVER.to_string());
        assert_eq!(take_env(), ShadowCfg { parent: 42, idle_s: SHADOW_IDLE_NEVER }, "no timeout");
        env.set(ENV_SHADOW_OF, "0");
        env.set(ENV_SHADOW_IDLE_S, "2");
        assert_eq!(take_env(), ShadowCfg::default());
        env.set(ENV_SHADOW_OF, "nonsense");
        assert_eq!(take_env(), ShadowCfg::default());
    }

    /// An idle timeout from an older core (0) is the default one, and a
    /// timeout of "never" keeps no deadline at all.
    #[test]
    fn shadow_session_idle_zero_is_the_default_and_never_keeps_no_deadline() {
        assert_eq!(ShadowCfg::new(7, 0), ShadowCfg { parent: 7, idle_s: SHADOW_IDLE_DEFAULT_S });
        assert_eq!(ShadowCfg::new(0, SHADOW_IDLE_NEVER), ShadowCfg::default());
        let never = Shadow::new(9, ShadowCfg::new(7, SHADOW_IDLE_NEVER), None);
        assert_eq!(never.due_in(), None);
        assert!(!never.times_out());
        let default = Shadow::new(9, ShadowCfg::new(7, 0), None);
        assert!(default.due_in().is_some_and(|d| d > Duration::from_secs(u64::from(SHADOW_IDLE_DEFAULT_S) - 60)));
    }
}
