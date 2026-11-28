//! codex — the OpenAI CLI, given the same pane affordances claudecode
//! has (RFC-008).
//!
//! Deliberately NOT a copy of `claudecode.rs`.  What the two agents
//! share is already parameterised: the pane→process binding takes a
//! predicate (`looks_like_*`), badges go through `PluginHost`, and the
//! registry dispatches to whoever claims a session.  So this plugin
//! supplies the parts that genuinely differ — how to recognise the
//! process, and what its badge says — and inherits the rest.
//!
//! What is deliberately absent for now: claudecode's profile-cycle
//! (SIGTERM, await-quiet, relaunch with `--resume`) leans on claude's
//! session-resume semantics, and codex's equivalent has not been
//! established.  Guessing at it would put a plugin in a position to
//! kill the user's agent mid-task.

mod alloc;
mod alloc_decide;
mod alloc_rebuild;
mod alloc_rollout;
#[cfg(test)]
mod alloc_tests;
#[cfg(test)]
mod alloc_watch_tests;
mod alloc_wire;
#[cfg(test)]
mod alloc_wire_tests;
mod badge;
mod cold_record;
mod cold_restore;
mod declare;
mod pane;
pub(in crate::plugins) mod context;
mod hand_off;
mod installer;
pub(in crate::plugins) mod last_model;
mod refresh;
mod rollout;
mod switch;
mod update;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod menu_tests;
#[cfg(test)]
mod switch_tests;
#[cfg(test)]
mod first_frame_tests;
pub(crate) use update::{install as install_cli, installed_cli, path_with_local_bin, tail};

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata,
            PLUGIN_API_VERSION};
use crate::plugins::{handoff, pidtree};
use marspot::ui::strings::{self, Arg};

use badge::badge_text;
use pane::{home_from, profile_from, PaneCodex};
use rollout::{codex_home, read_model_and_effort, RolloutIndex};

pub(crate) use pane::{argv_thread_id, home_of, profile_dirs};
pub(crate) use rollout::newest_rollout_for_cwd;

/// Is this descendant the `codex` CLI?
///
/// Same argv[0] approach `looks_like_claudecode` needs: a released
/// codex renames its process, so `comm` is not dependable.  The
/// basename must match exactly — `codex-code-mode-host` is a CHILD
/// helper codex spawns, and treating it as the agent would bind a
/// pane to the wrong pid and badge it twice.
pub fn looks_like_codex(d: &pidtree::ProcRow) -> bool {
    let Some(line) = pidtree::proc_cmdline(d.pid) else {
        return false;
    };
    let Some(argv0) = line.split(' ').next() else {
        return false;
    };
    argv0.rsplit('/').next().unwrap_or(argv0) == "codex"
}

/// The codex a pane is running, by the rule in
/// [`Session::agent`](crate::observe::Session::agent): the program in
/// front of the pane, and nothing a program there started.
pub(crate) fn pane_codex(s: &crate::observe::Session) -> Option<&crate::observe::Proc> {
    s.agent(|p| p.name() == "codex")
}

pub struct CodexPlugin {
    initialised: bool,
    /// Last badge published per session, so an unchanged scan does not
    /// republish — a badge write invalidates the pane's render cache.
    last_badge: std::collections::HashMap<u64, String>,
    rollouts: RolloutIndex,
    /// What each codex pane is running, for the badge menu.
    panes: std::collections::HashMap<u64, PaneCodex>,
    /// `CODEX_HOME` of each codex process, read once per process.
    env: crate::observe::EnvCache,
    follower: update::Follower,
    refresher: update::Refresher,
    /// The pane with the cursor; None until marspot has said.
    focused: Option<u64>,
    switches: switch::Switches,
    alloc: alloc_wire::AllocPass,
    /// The codex processes seen in front of panes, for `launch.observed`.
    seen: crate::plugins::launches::Seen,
    cold: cold_restore::Cold,
}

impl CodexPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: std::collections::HashMap::new(),
            rollouts: RolloutIndex::default(),
            panes: std::collections::HashMap::new(),
            env: crate::observe::EnvCache::default(),
            follower: update::Follower::default(),
            refresher: update::Refresher::default(),
            focused: None,
            switches: Default::default(),
            alloc: Default::default(),
            seen: Default::default(),
            cold: Default::default(),
        }
    }
}

impl Default for CodexPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CodexPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "codex",
            api_version: PLUGIN_API_VERSION,
            // Matches claudecode's cadence.  The badge only moves when
            // the user changes model or effort, so anything faster
            // would be spending CPU to watch a file that rarely moves.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        self.load_last_bound(host);
        self.initialised = true;
        Ok(())
    }

    fn on_pane_focused(&mut self, _host: &dyn PluginHost, shelld_session_id: u64) {
        self.focused = Some(shelld_session_id);
    }

    /// Right-click on the badge: pick the account profile.
    ///
    /// The pane is the unit — one pane's choice must not move the
    /// global config every other pane starts from.
    ///
    /// Profiles, not reasoning effort.  Effort is one `-c` override
    /// away and codex has its own key for it; which ACCOUNT a pane is
    /// talking to is the thing a terminal is in a position to know and
    /// the user has no other one-click way to change.  Whatever
    /// profiles exist are listed, and if that is one, it is one.
    fn pane_badge_menu(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        let Some(pane) = self.panes.get(&shelld_session_id) else {
            // A badge with nothing behind it: say so rather than open
            // an empty menu, which is indistinguishable from a click
            // that missed.
            host.log(
                LogLevel::Warn,
                "badge_menu.no_codex",
                &format!("shelld_session={shelld_session_id} has a badge but no codex"),
            );
            return Vec::new();
        };
        let dirs = profile_dirs();
        if dirs.is_empty() {
            host.log(
                LogLevel::Warn,
                "badge_menu.no_profiles",
                "no ~/.codex-profile-N directories; nothing to switch between",
            );
            return Vec::new();
        }
        let current = pane.profile;
        let mut rows: Vec<_> = dirs.iter()
            .map(|(n, _)| marspot::shell_proto::PaneBadgeMenuItem {
                // The tag carries the profile number, so a menu built
                // from one directory listing and acted on against
                // another (a profile created between the two) cannot
                // pick the wrong row by index.
                tag: handoff::profile_tag(handoff::Agent::Codex, *n),
                label: strings::text(Arg::ProfileRow { n: *n, current: current == Some(*n) }),
            })
            .collect();
        rows.extend(handoff::menu_rows(handoff::Agent::Codex));
        rows
    }

    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        // every plugin hears every pick; only codex's own rows are read
        let profile = match handoff::parse_tag(handoff::Agent::Codex, tag) {
            None => return,
            Some(handoff::Pick::HandOff(to, n)) => {
                if self.panes.contains_key(&shelld_session_id) {
                    self.hand_off(host, shelld_session_id, to, n);
                }
                return;
            }
            Some(handoff::Pick::Profile(n)) => n,
        };
        let Some((_, dir)) = profile_dirs().into_iter().find(|(n, _)| *n == profile) else {
            // The profile went away between the menu opening and the
            // click.  Say so: silently doing nothing is the failure
            // shape this codebase keeps having to dig out again.
            host.log(
                LogLevel::Warn,
                "profile.gone",
                &format!("profile {profile} no longer exists; not switching"),
            );
            return;
        };
        self.switch_profile(host, shelld_session_id, profile, dir);
    }

    /// With no codex in a pane and no badge left to clear, a tick does
    /// only what the CLI follower has due; a codex starting in a pane
    /// changes what the observer sees, and its knock brings the tick.
    fn tick_due(&self) -> super::TickDue {
        if !self.initialised {
            return super::TickDue::Event;
        }
        let obs = crate::observe::shared().latest();
        if !self.last_badge.is_empty() || !self.panes.is_empty() || obs.live().any(|s| pane_codex(s).is_some()) || self.cold_pending(&obs) {
            return super::TickDue::Cadence;
        }
        self.follower.due_at().map_or(super::TickDue::Cadence, super::TickDue::At)
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        // The observer's view, up to its interval old; this runs on the
        // main thread, which must not walk the process table.  With no
        // codex anywhere and none left to clear there is nothing to do.
        self.follow_switches(host);
        let obs = crate::observe::shared().latest();
        self.env.prune(&obs);
        // the CLI follows the daemon whether or not a pane runs codex
        if let Some(home) = codex_home() {
            self.follower.tick(&home);
        }
        let live = |sid: u64| obs.sessions.iter().any(|s| s.entry.id == sid);
        if self.last_badge.is_empty() && !obs.live().any(|s| pane_codex(s).is_some()) {
            self.cold_tick(host, &obs, &[], &std::collections::HashSet::new());
            self.seen.end_pass(live);
            self.refresh_stale(host, &[]);
            return;
        }
        let Some(home) = codex_home() else { return };
        self.rollouts.begin_tick();
        // The global config is the FALLBACK, not the answer: it says
        // what a fresh codex would start with.  Each pane's own
        // session is asked below.
        let (cfg_model, cfg_effort) = read_model_and_effort(&home);
        let mut codex_panes = Vec::new();
        let mut alloc_seen = Vec::new();
        let mut threads_bound = Vec::new();
        let mut with_codex = std::collections::HashSet::new();
        let views = crate::plugins::peerbus::threads();

        // Sessions come from the registry, not from pane indices: a
        // pane index is a position in a layout that moves when panes
        // are dragged or closed, while the session id is what a badge
        // is addressed to.  claudecode's scan reads the same source.
        for s in &obs.sessions {
            let sid = s.entry.id;
            let shell = s.entry.shell_child_pid;
            if shell <= 0 {
                continue;
            }
            let codex = pane_codex(s);
            let codex_id = codex.map(|p| p.row.id());
            let codex_pid = codex_id.map(|id| id.pid);
            let has_codex = codex_pid.is_some();
            // one read per process: its environment does not change
            let codex_env = codex.map(|p| self.env.value(&p.row, "CODEX_HOME"));
            // Ask THIS pane's session what it is running, and fall
            // back to a config when its record cannot be found (a
            // session that has not written a turn yet).  The config is
            // this pane's own: panes run different accounts, and the
            // supervisor's `CODEX_HOME` is one of them at best.
            let pane_home = codex_env.clone().and_then(home_from).unwrap_or_else(|| home.clone());
            let (pane_model, pane_effort) = if pane_home == home {
                (cfg_model.clone(), cfg_effort.clone())
            } else {
                read_model_and_effort(&pane_home)
            };
            let cwd = codex_pid.and_then(pidtree::proc_cwd).map(|c| c.to_string_lossy().into_owned());
            let facts = cwd.as_deref().and_then(|cwd| self.rollouts.facts_for_cwd(&pane_home, cwd));
            let (model, effort) = match facts {
                Some(f) => (
                    f.model.or_else(|| pane_model.clone()),
                    f.effort.or_else(|| pane_effort.clone()),
                ),
                None => (pane_model, pane_effort),
            };
            let profile = codex_env.and_then(profile_from);
            let text = self.hinted(sid, badge_text(profile, model.as_deref(), effort.as_deref()));
            if let Some(p) = codex {
                with_codex.insert(sid);
                // the thread by peerbus's binding of this very process,
                // else the one its command line resumes
                let thread = views.get(&sid).filter(|v| v.codex == p.row.id()).map(|v| (v.thread.clone(), v.home.clone()));
                let thread = thread.or_else(|| p.argv.as_deref().and_then(cold_record::argv_thread).map(|t| (t.to_string(), pane_home.clone())));
                if let Some((thread, home)) = thread.and_then(|(t, h)| Some((t, h.to_str()?.to_string()))) {
                    threads_bound.push(cold_restore::Bound { sid, thread, home, cwd: cwd.clone(), model: model.clone() });
                }
                codex_panes.push((sid, p.row.id(), p.argv.as_deref().map(<[String]>::to_vec)));
                alloc_seen.push(alloc_wire::Seen::new(sid, p, &pane_home, cwd.clone(), &self.rollouts, model.clone(), profile));
            }
            if let Some(id) = codex_id {
                let account = crate::plugins::launches::account_of_home(codex.and_then(|p| self.env.value(&p.row, "CODEX_HOME")).as_deref());
                self.seen.saw(sid, "codex", id, &account);
                self.panes.insert(
                    sid,
                    PaneCodex {
                        codex: id,
                        shell_pid: shell,
                        profile,
                        argv: codex.and_then(|p| p.argv.as_deref()).map(<[String]>::to_vec),
                    },
                );
            } else {
                self.panes.remove(&sid);
            }
            declare::declare(host, &mut self.last_badge, sid, has_codex, &text);
        }
        self.cold_tick(host, &obs, &threads_bound, &with_codex);
        self.seen.end_pass(live);
        self.refresh_stale(host, &codex_panes);
        self.alloc.run(host, &alloc_seen, &obs);
        super::helpers::used::look_at_codex();
    }
}

