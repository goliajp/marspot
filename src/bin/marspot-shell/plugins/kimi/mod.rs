//! kimi — Moonshot's Kimi Code CLI, given the pane affordances the
//! other agents have: a badge that says which account and model a pane
//! is on, and a report of whether its turn is running.
//!
//! Everything is read from what kimi itself writes.  Its command line
//! says nothing: a kimi started with arguments retitles itself and
//! zeroes them, so the session comes from the lock file that names the
//! process, and the model and the state from that session's record.
//! A record that cannot be read leaves the pane with its bare badge
//! and no report.

mod badge;
mod pane;
mod session;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use marspot::pane_state::Activity;
use marspot_peer::registry::Status;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use crate::observe::Proc;
use crate::plugins::pidtree;

use badge::{badge_text, publish_badge};
use pane::{home_from, profile_of};
use session::{Facts, SessionIndex};

/// The name the kernel has for the process.  Not argv[0]: that reads
/// `kimi-code` once kimi has retitled itself, and `kimi` before.
const COMM: &str = "kimi";

fn is_kimi(p: &Proc) -> bool {
    p.row.comm == COMM
}

/// The kimi a pane is running.  One that runs under another agent is
/// that agent's tool, not the pane's agent, and is left alone.
fn agent_in(descendants: &[Proc]) -> Option<&Proc> {
    let by_pid = |pid: i32| descendants.iter().find(|p| p.row.pid == pid);
    let mut kimi = descendants.iter().find(|p| is_kimi(p))?;
    let mut above = by_pid(kimi.row.ppid);
    while let Some(p) = above {
        if matches!(p.name(), "claude" | "codex") || crate::plugins::gemini::is_gemini(p) {
            return None;
        }
        if is_kimi(p) {
            kimi = p; // a kimi started from inside kimi: the outer one is the pane's
        }
        above = by_pid(p.row.ppid);
    }
    Some(kimi)
}

/// What the shell's state machine is told for a session state.
fn activity_of(status: Option<Status>) -> Activity {
    match status {
        Some(Status::Busy) => Activity::Working,
        Some(Status::Waiting) => Activity::ToolPending { executing: false },
        Some(Status::Idle) => Activity::AwaitingUser,
        // no session yet, or a record of a shape this does not know
        Some(Status::Unknown) | None => Activity::Unknown,
    }
}

static ACTIVITY: Mutex<Option<HashMap<u64, Activity>>> = Mutex::new(None);

/// What the kimi in pane `sid` is doing; None for a pane without one.
///
/// The shell keeps one activity per pane, and the claudecode plugin
/// reports every pane, `Absent` where there is no claude.  A second
/// reporter would take turns with it, so that plugin asks here before
/// it says `Absent`.
pub(crate) fn activity(sid: u64) -> Option<Activity> {
    ACTIVITY.lock().unwrap().as_ref()?.get(&sid).copied()
}

/// One pane's kimi as the last tick saw it.
#[derive(Clone, Debug, PartialEq)]
struct PaneKimi {
    home: PathBuf,
    session: Option<String>,
    facts: Option<Facts>,
}

pub struct KimiPlugin {
    initialised: bool,
    last_badge: HashMap<u64, String>,
    sessions: SessionIndex,
    panes: HashMap<u64, PaneKimi>,
    /// `KIMI_CODE_HOME` of each kimi process, read once per process.
    env: crate::observe::EnvCache,
    /// `default_model` of each kimi's home, read once per process: it
    /// is shown only until the first reply names the model in use.
    defaults: HashMap<pidtree::ProcId, Option<String>>,
}

impl KimiPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: HashMap::new(),
            sessions: SessionIndex::default(),
            panes: HashMap::new(),
            env: crate::observe::EnvCache::default(),
            defaults: HashMap::new(),
        }
    }

    fn see(&mut self, host: &dyn PluginHost, s: &crate::observe::Session, user_home: &Path) -> Option<u64> {
        if s.entry.shell_child_pid <= 0 {
            return None;
        }
        let kimi = agent_in(&s.descendants)?;
        let (sid, proc) = (s.entry.id, kimi.row.id());
        let home = home_from(self.env.value(&kimi.row, "KIMI_CODE_HOME"), user_home);
        let (session, facts) = match self.sessions.look(proc, &home) {
            Some((id, facts)) => (Some(id), facts),
            None => (None, None),
        };
        let model = match facts.as_ref().and_then(|f| f.model.clone()) {
            Some(m) => Some(m),
            None => self.defaults.entry(proc).or_insert_with(|| session::default_model(&home)).clone(),
        };
        publish_badge(host, &mut self.last_badge, sid, &badge_text(profile_of(&home), model.as_deref()));
        let now = PaneKimi { home, session, facts };
        if self.panes.get(&sid) != Some(&now) {
            host.log(LogLevel::Info, "pane", &format!("sid={sid} pid={} {}", proc.pid, describe(&now)));
            self.panes.insert(sid, now);
        }
        Some(sid)
    }
}

/// One log line for a pane whose kimi changed session or state.
fn describe(p: &PaneKimi) -> String {
    let session = p.session.as_deref().unwrap_or("none");
    let state = match &p.facts {
        Some(f) => format!(
            "status={} class={} failure={}",
            match f.status {
                Status::Idle => "idle",
                Status::Busy => "busy",
                Status::Waiting => "waiting",
                Status::Unknown => "unknown",
            },
            f.class.as_str(),
            f.failure.as_ref().map_or("none".to_string(), |x| format!("{}@turn{}", x.code, x.turn)),
        ),
        None => "status=unknown".to_string(),
    };
    format!("home={} session={session} {state}", p.home.display())
}

impl Default for KimiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for KimiPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "kimi",
            api_version: PLUGIN_API_VERSION,
            // the cadence of the other agents' plugins
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, _host: &dyn PluginHost) -> Result<(), PluginError> {
        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        // The observer's view, up to its interval old; this runs on the
        // main thread, which must not walk the process table.
        let obs = crate::observe::shared().latest();
        self.env.prune(&obs);
        let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
        let seen: Vec<u64> = obs.sessions.iter().filter_map(|s| self.see(host, s, &user_home)).collect();
        self.sessions.retain(|id| obs.has(id));
        self.defaults.retain(|id, _| obs.has(*id));
        self.panes.retain(|sid, _| seen.contains(sid));
        let activity = self
            .panes
            .iter()
            .map(|(sid, p)| (*sid, activity_of(p.facts.as_ref().map(|f| f.status))))
            .collect();
        *ACTIVITY.lock().unwrap() = Some(activity);
        // kimi left these panes: clear the badge this plugin set, and
        // only that one — another plugin may own the pane now.
        let gone: Vec<u64> = self.last_badge.keys().copied().filter(|sid| !self.panes.contains_key(sid)).collect();
        for sid in gone {
            self.last_badge.remove(&sid);
            let _ = host.set_pane_badge(sid, "");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn proc(pid: i32, ppid: i32, comm: &str, argv: &[&str]) -> Proc {
        let argv: Arc<[String]> = argv.iter().map(|a| a.to_string()).collect::<Vec<_>>().into();
        Proc {
            row: pidtree::ProcRow {
                pid,
                ppid,
                comm: comm.into(),
                start_unix: 0,
                start_us: pid as u64,
                pgid: pid,
                tty_dev: 1,
                tty_fg_pgid: 1,
                status: libc::SSLEEP,
            },
            argv: Some(argv),
        }
    }

    /// Started with arguments, kimi shows `kimi-code` and empty
    /// arguments; the kernel's name for it is what stays.
    #[test]
    fn kimi_is_known_by_the_kernel_s_name_not_its_title() {
        let procs = [proc(10, 1, "zsh", &["-zsh"]), proc(11, 10, "kimi", &["kimi-code", "", "", ""])];
        assert_eq!(agent_in(&procs).map(|p| p.row.pid), Some(11));
        let plain = [proc(11, 10, "kimi", &["kimi"])];
        assert_eq!(agent_in(&plain).map(|p| p.row.pid), Some(11));
        let look_alike = [proc(11, 10, "node", &["kimi"]), proc(12, 10, "kimi-legacy", &["kimi"])];
        assert!(agent_in(&look_alike).is_none());
    }

    #[test]
    fn a_kimi_run_by_another_agent_is_not_the_pane_s_agent() {
        for (comm, argv) in [("claude", &["claude"][..]), ("codex", &["codex"][..]), ("node", &["node", "/opt/homebrew/bin/gemini"][..])] {
            let procs = [proc(10, 1, comm, argv), proc(11, 10, "zsh", &["/bin/zsh", "-c", "kimi -p hi"]), proc(12, 11, "kimi", &["kimi-code", "", ""])];
            assert!(agent_in(&procs).is_none(), "{comm}");
        }
    }

    #[test]
    fn a_session_state_maps_onto_the_shell_s_activity() {
        assert_eq!(activity_of(Some(Status::Busy)), Activity::Working);
        assert_eq!(activity_of(Some(Status::Waiting)), Activity::ToolPending { executing: false });
        assert_eq!(activity_of(Some(Status::Idle)), Activity::AwaitingUser);
        assert_eq!(activity_of(None), Activity::Unknown, "nothing is assumed of a kimi with no session");
    }
}
