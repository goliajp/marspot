//! kimi — Moonshot's Kimi Code CLI, given the pane affordances the
//! other agents have: a badge that says which account, model and
//! thinking effort a pane is on, a report of whether its turn is
//! running, and a place among the sessions that message each other.
//!
//! Everything is read from what kimi itself writes.  Its command line
//! says nothing: a kimi started with arguments retitles itself and
//! zeroes them, so the session comes from the lock file that names the
//! process, and the model, the effort and the state from that
//! session's record.  A record that cannot be read leaves the pane
//! with its bare badge and no report.

mod badge;
mod config;
mod pane;
mod peer;
mod session;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use marspot::pane_state::Activity;
use marspot_peer::envelope::Mode;
use marspot_peer::registry::Status;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use crate::observe::Proc;
use crate::plugins::{pidtree, pty_op};

use badge::{badge_text, publish_badge};
use pane::{home_from, profile_of};
use session::{Facts, SessionIndex, ENV_ALIAS};

pub(crate) use pane::{default_home, profile_dirs_in};
pub(crate) use peer::{peers, Peer, Peers};

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

/// The model and the effort a pane's badge shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Shown {
    model: Option<String>,
    thinking: Option<String>,
}

/// A model's name without the provider kimi files it under:
/// `kimi-code/k3` is `k3`.
fn short(alias: &str) -> &str {
    alias.split_once('/').map_or(alias, |(_, model)| model)
}

/// What a session's record says the next request uses.  `env_model` is
/// the process's `KIMI_MODEL_NAME`, which names a model that has no
/// alias of its own.
fn shown_of(facts: &Facts, env_model: Option<&str>) -> Shown {
    let model = match facts.alias.as_deref() {
        Some(ENV_ALIAS) => facts.model.clone().or_else(|| env_model.map(str::to_string)),
        Some(alias) => Some(short(alias).to_string()),
        None => None,
    };
    Shown { model, thinking: facts.thinking.clone() }
}

/// What a kimi with no session yet will use, as far as it can be told.
///
/// A kimi started with arguments may have been given `-m`, and its
/// arguments are gone; nothing is said for it.  One started bare takes
/// the model from the environment or from its configuration, in that
/// order, and the effort from its configuration.
fn shown_before_a_session(bare: bool, env_model: Option<&str>, home: &Path) -> Shown {
    if !bare {
        return Shown::default();
    }
    if let Some(model) = env_model {
        return Shown { model: Some(model.to_string()), thinking: None };
    }
    let d = config::defaults(home);
    Shown { model: d.alias.as_deref().map(|a| short(a).to_string()), thinking: d.thinking }
}

/// One pane's kimi as the last tick saw it.
#[derive(Clone, Debug, PartialEq)]
struct PaneKimi {
    proc: pidtree::ProcId,
    home: PathBuf,
    cwd: Option<String>,
    session: Option<String>,
    facts: Option<Facts>,
    shown: Shown,
}

pub struct KimiPlugin {
    initialised: bool,
    last_badge: HashMap<u64, String>,
    sessions: SessionIndex,
    panes: HashMap<u64, PaneKimi>,
    /// `KIMI_CODE_HOME` and `KIMI_MODEL_NAME` of each kimi process,
    /// read once per process.
    env: crate::observe::EnvCache,
    /// What each kimi shows before it has a session, worked out once
    /// per process: it is shown only until the first prompt.
    before: HashMap<pidtree::ProcId, Shown>,
    /// Panes that were woken for the mail now waiting, so one inbox is
    /// one line typed.
    woken: HashSet<u64>,
}

impl KimiPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: HashMap::new(),
            sessions: SessionIndex::default(),
            panes: HashMap::new(),
            env: crate::observe::EnvCache::default(),
            before: HashMap::new(),
            woken: HashSet::new(),
        }
    }

    fn see(&mut self, host: &dyn PluginHost, s: &crate::observe::Session, user_home: &Path) -> Option<u64> {
        if s.entry.shell_child_pid <= 0 {
            return None;
        }
        let kimi = agent_in(&s.descendants)?;
        let (sid, proc) = (s.entry.id, kimi.row.id());
        let home = home_from(self.env.value(&kimi.row, "KIMI_CODE_HOME"), user_home);
        let env_model = self.env.value(&kimi.row, "KIMI_MODEL_NAME").filter(|m| config::plain(m));
        let (session, facts) = match self.sessions.look(proc, &home) {
            Some((id, facts)) => (Some(id), facts),
            None => (None, None),
        };
        let shown = match &facts {
            Some(f) => shown_of(f, env_model.as_deref()),
            None => {
                let bare = kimi.argv.as_ref().is_some_and(|a| a.len() == 1);
                self.before.entry(proc).or_insert_with(|| shown_before_a_session(bare, env_model.as_deref(), &home)).clone()
            }
        };
        let text = badge_text(profile_of(&home), shown.model.as_deref(), shown.thinking.as_deref());
        publish_badge(host, &mut self.last_badge, sid, &text);
        let cwd = pidtree::proc_cwd(proc.pid).map(|c| c.to_string_lossy().into_owned());
        let now = PaneKimi { proc, home, cwd, session, facts, shown };
        if self.panes.get(&sid) != Some(&now) {
            host.log(LogLevel::Info, "pane", &format!("sid={sid} pid={} {}", proc.pid, describe(&now)));
            self.panes.insert(sid, now);
        }
        Some(sid)
    }

    /// Type the wake line into every idle pane that has mail and has
    /// not been woken for it.
    ///
    /// Only where the session's record says idle, which rules out a
    /// turn and a pending approval, and the screen shows kimi's input
    /// box with nothing typed in it, which rules out a dialog and the
    /// person's own half-written line.  Once per inbox: a pane stays
    /// marked until its mail has been taken, so a hook that does not
    /// run is not typed at again.
    fn wake(&mut self, host: &dyn PluginHost) {
        for (sid, pane) in &self.panes {
            let idle = pane.facts.as_ref().is_some_and(|f| f.status == Status::Idle);
            if !idle || !crate::plugins::peerbus::kimi_mail_waiting(*sid) {
                self.woken.remove(sid);
                continue;
            }
            if self.woken.contains(sid) || !matches!(marspot::pane_read::composer_box(*sid), Ok(Some(typed)) if typed.is_empty()) {
                continue;
            }
            match host.submit_pty_op(*sid, wake_op()) {
                Ok(()) => {
                    self.woken.insert(*sid);
                    host.log(LogLevel::Info, "wake", &format!("sid={sid}: mail is waiting for an idle session"));
                }
                Err(e) => host.log(LogLevel::Warn, "wake.submit_failed", &format!("sid={sid}: {e}")),
            }
        }
        self.woken.retain(|sid| self.panes.contains_key(sid));
    }
}

/// The wake line typed and sent.  In two writes with a pause between:
/// a line that arrives together with its return reads as pasted text.
fn wake_op() -> pty_op::PtyOp {
    pty_op::PtyOp::new("kimi.wake")
        .step(pty_op::Step::send(peer::WAKE_LINE.as_bytes().to_vec()).named("type"))
        .step(pty_op::Step::settle(Duration::from_millis(300)).named("type_settle"))
        .step(pty_op::Step::send(b"\r".to_vec()).named("submit"))
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
            f.failure.as_ref().map_or("none".to_string(), |x| format!("{}@turn{}", x.name.as_deref().unwrap_or(&x.code), x.turn)),
        ),
        None => "status=unknown".to_string(),
    };
    let (model, thinking) = (p.shown.model.as_deref().unwrap_or("?"), p.shown.thinking.as_deref().unwrap_or("?"));
    format!("home={} session={session} model={model} thinking={thinking} {state}", p.home.display())
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
        self.before.retain(|id, _| obs.has(*id));
        self.panes.retain(|sid, _| seen.contains(sid));
        let activity = self.panes.iter().map(|(sid, p)| (*sid, activity_of(p.facts.as_ref().map(|f| f.status)))).collect();
        *ACTIVITY.lock().unwrap() = Some(activity);

        let mut peers = Peers { bound: Vec::new(), all_known: true };
        for (sid, p) in &self.panes {
            match (&p.session, &p.cwd) {
                (Some(session), Some(cwd)) => peers.bound.push(Peer {
                    sid: *sid,
                    kimi: p.proc,
                    session: session.clone(),
                    cwd: cwd.clone(),
                    status: p.facts.as_ref().map_or(Status::Unknown, |f| f.status),
                    class: p.facts.as_ref().map_or(Mode::Prompting, |f| f.class),
                }),
                // no session before the first prompt: nothing to bind
                (None, _) => {}
                (Some(_), None) => peers.all_known = false,
            }
        }
        peers.bound.sort_by_key(|p| p.sid);
        peer::publish(peers);
        self.wake(host);

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

    fn facts(alias: Option<&str>, model: Option<&str>, thinking: Option<&str>) -> Facts {
        Facts {
            alias: alias.map(str::to_string),
            model: model.map(str::to_string),
            thinking: thinking.map(str::to_string),
            status: Status::Idle,
            class: Mode::Prompting,
            failure: None,
        }
    }

    #[test]
    fn a_session_shows_the_model_it_is_bound_to() {
        let s = |m: Option<&str>, t: Option<&str>| Shown { model: m.map(str::to_string), thinking: t.map(str::to_string) };
        assert_eq!(shown_of(&facts(Some("kimi-code/k3"), Some("k3"), Some("high")), None), s(Some("k3"), Some("high")));
        assert_eq!(shown_of(&facts(Some("kimi-code/k3"), None, Some("low")), None), s(Some("k3"), Some("low")), "before its first request");
        assert_eq!(shown_of(&facts(Some("plain"), None, None), None), s(Some("plain"), None));
        // a model defined by environment variables has no alias to show
        assert_eq!(shown_of(&facts(Some(ENV_ALIAS), Some("mock-model"), Some("on")), None), s(Some("mock-model"), Some("on")));
        assert_eq!(shown_of(&facts(Some(ENV_ALIAS), None, None), Some("from-env")), s(Some("from-env"), None));
        assert_eq!(shown_of(&facts(None, None, None), Some("from-env")), s(None, None), "nothing is made up");
    }

    #[test]
    fn before_a_session_only_a_bare_kimi_s_defaults_are_known() {
        let home = crate::plugins::handoff::history::tmp("kimi-before");
        std::fs::write(
            home.join("config.toml"),
            "default_model = \"kimi-code/k3\"\n\n[models.\"kimi-code/k3\"]\ncapabilities = [ \"thinking\" ]\nsupport_efforts = [ \"low\", \"high\" ]\ndefault_effort = \"high\"\n",
        )
        .unwrap();
        let s = |m: Option<&str>, t: Option<&str>| Shown { model: m.map(str::to_string), thinking: t.map(str::to_string) };
        assert_eq!(shown_before_a_session(true, None, &home), s(Some("k3"), Some("high")));
        assert_eq!(shown_before_a_session(true, Some("mock"), &home), s(Some("mock"), None), "the environment's model comes first");
        assert_eq!(shown_before_a_session(false, None, &home), s(None, None), "its arguments may have named another model");
    }
}
