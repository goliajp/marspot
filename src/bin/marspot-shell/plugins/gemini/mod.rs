//! gemini — Google's gemini-cli, given the pane affordances codex has:
//! a badge that says which account and model a pane is on, a menu to
//! move it to another account, and a handoff to and from the other
//! agents.
//!
//! The same rule the codex plugin states applies here: nothing is done
//! on a guess about the user's agent.  A pane whose gemini cannot be
//! tied to exactly one session file keeps its badge — the account comes
//! from the process itself — but offers nothing that needs the session.
//!
//! What is deliberately absent:
//!
//! - Activity reports.  The shell keeps one activity per pane and the
//!   claudecode plugin already answers `Absent` for every pane without
//!   claude, so a second reporter would only take turns with it.  The
//!   session file could not carry the report anyway: a reply is
//!   recorded before its tool calls are, so "the model has finished"
//!   and "a tool is waiting for approval" are the same last record.
//! - Automatic account switching.  gemini writes no usage figures to
//!   disk for a switch to be decided on.

mod badge;
mod hand_off;
mod pane;
mod session;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION};
use crate::observe::Proc;
use crate::plugins::{handoff, pidtree};

use badge::{badge_text, publish_badge};
use pane::{argv_facts, profile_of, profile_switch_op, root_from, ArgvFacts, PaneGemini};
use session::{Asker, SessionIndex};

pub(crate) use pane::{config_dir, handoff_homes, launch, profile_dirs};
pub(crate) use session::{by_id as session_by_id, head_id as session_id_of};
#[cfg(test)]
pub(crate) use session::fixture;

/// Is this argv the gemini CLI?
///
/// Two shapes.  The standalone build is an executable called `gemini`.
/// An npm install is `node` running a script called `gemini` (the bin
/// link) or `gemini.js` (what the link resolves to once gemini
/// relaunches itself); the script is the first argument that is not a
/// node flag.  Both names must match exactly, so a helper such as
/// `gemini-helper` is not taken for the agent.
fn argv_is_gemini(argv: &[String]) -> bool {
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let Some(argv0) = argv.first().map(|a| base(a)) else { return false };
    match argv0.as_str() {
        "gemini" => true,
        "node" => argv[1..]
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|script| matches!(base(script).as_str(), "gemini" | "gemini.js")),
        _ => false,
    }
}

/// Is this descendant the gemini CLI?
pub fn looks_like_gemini(d: &pidtree::ProcRow) -> bool {
    pidtree::proc_argv(d.pid).is_some_and(|argv| argv_is_gemini(&argv))
}

fn is_gemini(p: &Proc) -> bool {
    p.argv.as_deref().is_some_and(argv_is_gemini)
}

/// The gemini a pane is running: the one nearest the shell, and the one
/// doing the work.
///
/// gemini relaunches itself as a child of the same name, so a pane
/// normally holds two; they are one agent, bound once.  A gemini that
/// runs under claude or codex is that agent's tool, not the pane's
/// agent, and is left alone.
fn agent_in(descendants: &[Proc]) -> Option<(&Proc, pidtree::ProcId)> {
    let by_pid = |pid: i32| descendants.iter().find(|p| p.row.pid == pid);
    let outer = descendants
        .iter()
        .find(|p| is_gemini(p) && !by_pid(p.row.ppid).is_some_and(is_gemini))?;
    let mut above = by_pid(outer.row.ppid);
    while let Some(p) = above {
        if matches!(p.name(), "claude" | "codex") {
            return None;
        }
        above = by_pid(p.row.ppid);
    }
    let mut worker = outer;
    while let Some(child) = descendants.iter().find(|p| p.row.ppid == worker.row.pid && is_gemini(p)) {
        worker = child;
    }
    Some((outer, worker.row.id()))
}

/// One pane's gemini as this tick saw it, before its session is asked
/// for.
struct Seen {
    sid: u64,
    shell_pid: i32,
    gemini: pidtree::ProcId,
    worker: pidtree::ProcId,
    started: SystemTime,
    root: PathBuf,
    cwd: Option<String>,
    args: ArgvFacts,
}

pub struct GeminiPlugin {
    initialised: bool,
    /// Last badge published per session, for the log and for knowing
    /// which badges are this plugin's to clear.
    last_badge: std::collections::HashMap<u64, String>,
    sessions: SessionIndex,
    /// What each gemini pane is running, for the badge menu.
    panes: std::collections::HashMap<u64, PaneGemini>,
    /// `GEMINI_CLI_HOME` of each gemini process, read once per process.
    env: crate::observe::EnvCache,
}

impl GeminiPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: std::collections::HashMap::new(),
            sessions: SessionIndex::default(),
            panes: std::collections::HashMap::new(),
            env: crate::observe::EnvCache::default(),
        }
    }

    fn see(&mut self, s: &crate::observe::Session, user_home: &Path) -> Option<Seen> {
        let shell_pid = s.entry.shell_child_pid;
        if shell_pid <= 0 {
            return None;
        }
        let (outer, worker) = agent_in(&s.descendants)?;
        // one read per process: its environment does not change
        let root = root_from(self.env.value(&outer.row, "GEMINI_CLI_HOME"), user_home);
        Some(Seen {
            sid: s.entry.id,
            shell_pid,
            gemini: outer.row.id(),
            worker,
            started: SystemTime::UNIX_EPOCH + Duration::from_micros(outer.row.start_us),
            root,
            cwd: pidtree::proc_cwd(outer.row.pid).map(|c| c.to_string_lossy().into_owned()),
            args: argv_facts(outer.argv.as_deref().unwrap_or_default()),
        })
    }
}

impl Default for GeminiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for GeminiPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "gemini",
            api_version: PLUGIN_API_VERSION,
            // the cadence of the other two agents; the badge moves only
            // when the model or the account does
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, _host: &dyn PluginHost) -> Result<(), PluginError> {
        self.initialised = true;
        Ok(())
    }

    /// Right-click on the badge: the account profiles, then the other
    /// agents this conversation can be handed to.  A pane that is not
    /// gemini's gets no rows from here; every plugin is asked about
    /// every pane.
    fn pane_badge_menu(
        &mut self,
        _host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        let Some(pane) = self.panes.get(&shelld_session_id) else {
            return Vec::new();
        };
        let mut rows: Vec<_> = profile_dirs()
            .iter()
            .map(|(n, _)| marspot::shell_proto::PaneBadgeMenuItem {
                // the tag IS the profile number, so a profile created
                // between the menu and the click cannot shift the rows
                tag: *n as u32,
                label: if pane.profile == Some(*n) { format!("● profile {n}") } else { format!("   profile {n}") },
            })
            .collect();
        rows.extend(handoff::menu_rows(handoff::Agent::Gemini));
        rows
    }

    fn on_pane_badge_menu_action(&mut self, host: &dyn PluginHost, shelld_session_id: u64, tag: u32) {
        // every plugin hears every pick; one on another agent's pane is not ours
        let Some(pane) = self.panes.get(&shelld_session_id).cloned() else {
            return;
        };
        if let Some((to, n)) = handoff::parse_tag(tag) {
            if to != handoff::Agent::Gemini {
                self.hand_off(host, shelld_session_id, &pane, to, n);
            }
            return;
        }
        let Ok(profile) = u8::try_from(tag) else {
            return;
        };
        if pane.profile == Some(profile) {
            return; // already there; a stale menu is not a request
        }
        let warn = |tag: &str, why: &str| {
            host.log(LogLevel::Warn, tag, &format!("pane {shelld_session_id}: {why}; not switching"));
        };
        let Some((_, dir)) = profile_dirs().into_iter().find(|(n, _)| *n == profile) else {
            return warn("profile.gone", &format!("profile {profile} no longer exists"));
        };
        let (Some(session), Some(cwd)) = (pane.session.as_deref(), pane.cwd.as_deref()) else {
            return warn("cycle.no_session", "its session could not be told");
        };
        // Sessions live inside the home.  One that the target home does
        // not hold cannot be resumed there, and finding that out after
        // the running gemini was stopped would leave the pane with
        // neither.
        if session_by_id(&config_dir(&dir), cwd, session).is_none() {
            return warn("cycle.session_not_there", &format!("profile {profile} does not hold session {session}"));
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
        let Some(op) = profile_switch_op(&pane, profile, &dir, session, &home) else {
            return warn("cycle.no_command", "the resume line cannot be built");
        };
        if let Err(e) = host.submit_pty_op(shelld_session_id, op) {
            host.log(LogLevel::Warn, "cycle.submit_failed", &format!("pane {shelld_session_id}: {e}"));
        }
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
        let seen: Vec<Seen> = obs.sessions.iter().filter_map(|s| self.see(s, &user_home)).collect();
        let live: Vec<pidtree::ProcId> = seen.iter().map(|s| s.gemini).collect();
        self.sessions.retain(&live);
        self.panes.retain(|sid, _| seen.iter().any(|s| s.sid == *sid));

        for s in &seen {
            let cfg = config_dir(&s.root);
            // The newest file in a directory belongs to one process.  Two
            // that could each be its writer are both left unbound.
            let lone = seen
                .iter()
                .filter(|o| o.args.session.is_none() && o.root == s.root && o.cwd == s.cwd)
                .count()
                == 1;
            let facts = s.cwd.as_deref().and_then(|cwd| {
                self.sessions.facts(&Asker {
                    proc: s.gemini,
                    started: s.started,
                    config_dir: &cfg,
                    cwd,
                    session: s.args.session.as_deref(),
                    lone,
                })
            });
            let profile = profile_of(&s.root);
            let model = s.args.model.as_deref().or_else(|| facts.as_ref().and_then(|f| f.model.as_deref()));
            publish_badge(host, &mut self.last_badge, s.sid, &badge_text(profile, model));
            self.panes.insert(
                s.sid,
                PaneGemini {
                    worker: s.worker,
                    shell_pid: s.shell_pid,
                    root: s.root.clone(),
                    profile,
                    cwd: s.cwd.clone(),
                    session: facts.map(|f| f.session),
                },
            );
        }
        // gemini left these panes: clear the badge this plugin set, and
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

    fn proc(pid: i32, ppid: i32, argv: &[&str]) -> Proc {
        let argv: Arc<[String]> = argv.iter().map(|a| a.to_string()).collect::<Vec<_>>().into();
        Proc {
            row: pidtree::ProcRow {
                pid,
                ppid,
                comm: "x".into(),
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

    fn is(argv: &[&str]) -> bool {
        argv_is_gemini(&argv.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_standalone_build_and_the_npm_install_are_both_gemini() {
        assert!(is(&["gemini"]));
        assert!(is(&["/opt/gem/bin/gemini", "-m", "flash"]));
        // the relaunched child of the standalone build
        assert!(is(&["/opt/gem/bin/gemini", "/opt/gem/bin/gemini", "-m", "flash"]));
        assert!(is(&["node", "/opt/homebrew/bin/gemini"]));
        assert!(is(&[
            "/opt/homebrew/bin/node",
            "--max-old-space-size=32768",
            "/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js",
            "-r",
            "latest"
        ]));
    }

    #[test]
    fn a_look_alike_is_not_gemini() {
        assert!(!is(&["gemini-helper"]));
        assert!(!is(&["/usr/local/bin/gemini-helper", "gemini"]));
        assert!(!is(&["node", "/x/gemini-helper.js"]));
        assert!(!is(&["node", "/x/server.js", "gemini"]), "the script is what counts, not a later word");
        assert!(!is(&["node"]));
        assert!(!is(&["python3", "gemini"]));
        assert!(!is(&[]));
    }

    /// Neither of the other two plugins takes a gemini for its own:
    /// both match on this name.
    #[test]
    fn a_gemini_process_is_not_named_claude_or_codex() {
        for argv in [&["/opt/gem/bin/gemini"][..], &["node", "/opt/homebrew/bin/gemini"][..]] {
            let p = proc(10, 1, argv);
            assert!(!matches!(p.name(), "claude" | "codex"), "{argv:?}");
        }
    }

    #[test]
    fn the_parent_and_its_relaunched_child_are_one_agent() {
        let procs = [
            proc(10, 1, &["/opt/gem/bin/gemini", "-m", "flash"]),
            proc(11, 10, &["/opt/gem/bin/gemini", "/opt/gem/bin/gemini", "-m", "flash"]),
            proc(12, 11, &["/bin/zsh", "-c", "ls"]),
        ];
        let (outer, worker) = agent_in(&procs).expect("one agent");
        assert_eq!(outer.row.pid, 10, "bound to the one nearest the shell");
        assert_eq!(worker.pid, 11, "and the child is the one that works");

        // listed child first: the order of the table is not the tree
        let reversed = [procs[1].clone(), procs[0].clone()];
        let (outer, worker) = agent_in(&reversed).expect("one agent");
        assert_eq!((outer.row.pid, worker.pid), (10, 11));

        let single = [proc(10, 1, &["node", "/opt/homebrew/bin/gemini"])];
        let (outer, worker) = agent_in(&single).expect("one agent");
        assert_eq!((outer.row.pid, worker.pid), (10, 10), "no relaunch: one process is both");
    }

    #[test]
    fn a_gemini_run_by_another_agent_is_not_the_pane_s_agent() {
        let procs = [
            proc(10, 1, &["claude"]),
            proc(11, 10, &["/bin/zsh", "-c", "gemini -p hi"]),
            proc(12, 11, &["/opt/gem/bin/gemini", "-p", "hi"]),
        ];
        assert!(agent_in(&procs).is_none());
        assert!(agent_in(&[proc(10, 1, &["/bin/zsh"]), proc(11, 10, &["gemini-helper"])]).is_none());
    }
}
