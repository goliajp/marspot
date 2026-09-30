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

mod badge;
mod pane;
mod rollout;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PermissionSet,
            PLUGIN_API_VERSION};
use crate::plugins::{handoff, pidtree};

use badge::{badge_text, publish_badge};
use pane::{profile_of, profile_switch_op, PaneCodex};
use rollout::{codex_home, read_model_and_effort, RolloutIndex};

pub(crate) use pane::{argv_thread_id, home_of, profile_dirs};
pub(crate) use rollout::newest_rollout_for_cwd;

/// How the wheel reaches codex.  Verified by injecting into a real
/// pty: `Ctrl+T` opens the transcript, and from there codex takes both
/// `↑`/`↓` (one line) and `PgUp`/`PgDn` (one screen).
///
/// The wheel maps to the LINE keys, not the page keys.  A wheel notch
/// means a few lines — the caller already turns the trackpad's pixels
/// and the mouse's notches into an accelerated line count — so paging
/// per notch threw away a whole screen for one flick of the finger
/// (2026-09-06: "我们一下就滚一屏", against iTerm2 scrolling line by
/// line with acceleration).
///
/// Plain `CSI A`, not `SS3 A`: codex never turns on application cursor
/// keys (`CSI ? 1 h` appears zero times in a full session's byte log),
/// so the normal-mode encoding is the one it reads.
///
/// `Ctrl+T` is a TOGGLE — measured, a second one closes the view — so
/// L2 must never send it blind.  `WHEEL_MARKER` is the rule codex
/// draws across the top of that view (it survives paging), letting L2
/// read the state off the screen instead of remembering it.
const WHEEL_ENTER: &[u8] = b"\x14";
const WHEEL_UP: &[u8] = b"\x1b[A";
const WHEEL_DOWN: &[u8] = b"\x1b[B";
const WHEEL_MARKER: &[u8] = b"/TRANSCRIPT/";

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

pub struct CodexPlugin {
    initialised: bool,
    /// Last badge published per session, so an unchanged scan does not
    /// republish — a badge write invalidates the pane's render cache.
    last_badge: std::collections::HashMap<u64, String>,
    rollouts: RolloutIndex,
    /// What each codex pane is running, for the badge menu.
    panes: std::collections::HashMap<u64, PaneCodex>,
}

impl CodexPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: std::collections::HashMap::new(),
            rollouts: RolloutIndex::default(),
            panes: std::collections::HashMap::new(),
        }
    }
}

impl CodexPlugin {
    /// Move this pane's conversation to claude profile `n` (RFC-009).
    fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, n: u8) {
        let Some(pane) = self.panes.get(&sid).cloned() else {
            host.log(LogLevel::Warn, "handoff.no_codex", &format!("pane {sid}: no codex to hand off from"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(handoff::Agent::Claude).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("claude profile {n} no longer exists"));
            return;
        };
        let Some(home) = home_of(pane.codex_pid) else { return };
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Codex,
            pid: pane.codex_pid,
            shell_pid: pane.shell_pid,
            home,
            session_id: argv_thread_id(pane.codex_pid),
            cwd: pidtree::proc_cwd(pane.codex_pid).map(|c| c.to_string_lossy().into_owned()),
        };
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} codex → claude P{n}: {leaving:?}"));
        let Some(op) = handoff::switch_op(sid, leaving, handoff::Agent::Claude, n, dir) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the claude line"));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
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
            version: "0.1.0",
            api_version: PLUGIN_API_VERSION,
            permissions: PermissionSet::READ_PANE_INFO
                | PermissionSet::READ_PTY_TREE
                | PermissionSet::READ_DISK_FS
                | PermissionSet::SET_STATUS_LINE,
            // Matches claudecode's cadence.  The badge only moves when
            // the user changes model or effort, so anything faster
            // would be spending CPU to watch a file that rarely moves.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, _host: &dyn PluginHost) -> Result<(), PluginError> {
        self.initialised = true;
        Ok(())
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
                // The tag IS the profile number, so a menu built from
                // one directory listing and acted on against another
                // (a profile created between the two) cannot pick the
                // wrong row by index.
                tag: *n as u32,
                label: if current == Some(*n) {
                    format!("● profile {n}")
                } else {
                    format!("   profile {n}")
                },
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
        if let Some((to, n)) = handoff::parse_tag(tag) {
            if to == handoff::Agent::Claude {
                self.hand_off(host, shelld_session_id, n);
            }
            return;
        }
        let Ok(profile) = u8::try_from(tag) else {
            return; // another plugin's row
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
        let Some(pane) = self.panes.get(&shelld_session_id).cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.menu_no_codex",
                &format!("menu pick on shelld_session={shelld_session_id} with no codex"),
            );
            return;
        };
        if pane.profile == Some(profile) {
            return; // already there; a stale menu is not a request
        }
        let Some(op) = profile_switch_op(&pane, profile, &dir) else {
            return;
        };
        if let Err(e) = host.submit_pty_op(shelld_session_id, op) {
            host.log(
                LogLevel::Warn,
                "cycle.submit_failed",
                &format!("shelld_session={shelld_session_id}: {e}"),
            );
        }
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        let Some(home) = codex_home() else { return };
        self.rollouts.begin_tick();
        // The global config is the FALLBACK, not the answer: it says
        // what a fresh codex would start with.  Each pane's own
        // session is asked below.
        let (cfg_model, cfg_effort) = read_model_and_effort(&home);

        // Sessions come from the registry, not from pane indices: a
        // pane index is a position in a layout that moves when panes
        // are dragged or closed, while the session id is what a badge
        // is addressed to.  claudecode's scan reads the same source.
        let procs = pidtree::list_all_procs();
        for entry in marspot_term::session_registry::list_session_entries() {
            let sid = entry.id;
            let shell = entry.shell_child_pid;
            if shell <= 0 {
                continue;
            }
            let codex_pid = pidtree::descendants_of(shell, &procs)
                .into_iter()
                .find(looks_like_codex)
                .map(|p| p.pid);
            let has_codex = codex_pid.is_some();
            // Ask THIS pane's session what it is running, and fall
            // back to a config when its record cannot be found (a
            // session that has not written a turn yet).  The config is
            // this pane's own: panes run different accounts, and the
            // supervisor's `CODEX_HOME` is one of them at best.
            let pane_home = codex_pid.and_then(home_of).unwrap_or_else(|| home.clone());
            let (pane_model, pane_effort) = if pane_home == home {
                (cfg_model.clone(), cfg_effort.clone())
            } else {
                read_model_and_effort(&pane_home)
            };
            let facts = codex_pid
                .and_then(pidtree::proc_cwd)
                .and_then(|cwd| self.rollouts.facts_for_cwd(&pane_home, &cwd.to_string_lossy()));
            let (model, effort) = match facts {
                Some(f) => (
                    f.model.or_else(|| pane_model.clone()),
                    f.effort.or_else(|| pane_effort.clone()),
                ),
                None => (pane_model, pane_effort),
            };
            let text = badge_text(model.as_deref(), effort.as_deref());
            if let Some(pid) = codex_pid {
                self.panes.insert(
                    sid,
                    PaneCodex {
                        codex_pid: pid,
                        shell_pid: shell,
                        effort: effort.clone(),
                        profile: profile_of(pid),
                    },
                );
            } else {
                self.panes.remove(&sid);
            }
            if has_codex {
                // Declare how the wheel reaches codex.  Verified by
                // injecting into a real pty: `PageUp` alone changes
                // nothing, `Ctrl+T` opens its /TRANSCRIPT/ view, and
                // `PageUp`/`PageDown` page it from there.
                //
                // Nothing leaves the view: the user asked for the
                // wheel to take them in but never to throw them out,
                // since one stray tick at the bottom would otherwise
                // close what they were reading.  `Esc` stays theirs.
                //
                // `/TRANSCRIPT/` is the rule codex draws across the top
                // while that view is open, and it survives paging —
                // so L2 can read the state off the screen rather than
                // remember it.  That matters because `Ctrl+T` is a
                // TOGGLE: measured, a second one closes the view, so a
                // remembered flag going stale would shut the transcript
                // instead of opening it (2026-09-06: after leaving the
                // view, scrolling could not get back in).
                // codex does not render HTML, so a model that writes
                // `<u>…</u>` has its markup land on screen as text.
                // Declared per pane, never globally: a terminal is
                // where people TALK about markup, and switched on
                // everywhere it ate the tags out of the conversation
                // specifying this (2026-09-06).
                //
                // Re-issued every tick, like the badge and unlike the
                // wheel keys.  This one has to reach L3, and an L3
                // that is mid-execv when it arrives simply drops it —
                // which is exactly what happened the first time it
                // shipped: L1 declared 1.4 s after the session images
                // were swapped, and no pane ever heard it.  Repeating
                // costs one small frame every two seconds and makes
                // the restart window cost a tick instead of forever.
                // `<u>` rendering is NOT declared any more.  It was
                // added because codex printed its own `<u>` markup as
                // text; measured across 102 MB of real codex traffic
                // since, `<u>` appears TWICE (and `</u>` eight times —
                // not even paired), while `<h2>` and `<p>` appear 529
                // times because the model prints HTML documents into
                // the pane.  Swallowing a real document's tags is now
                // 250x more likely than fixing codex's own, so the
                // feature is net-negative here.  `render_u_tags` in
                // settings.toml still turns it on for anyone who wants
                // it everywhere.
                // Re-issued every tick on purpose: a core swap starts
                // L2 with an empty map, and a declaration sent once
                // never reaches it.  That is how a codex pane lost its
                // hard-wrap link merging on 2026-09-07 while the three
                // unwrapped paths beside it still worked.
                let _ = host.set_pane_agent_tui(sid, true);
                let _ = host.set_pane_render_markup(sid, false);
                // The wheel is NOT routed into codex's transcript any
                // more.  It was, because a codex pane had no history
                // of its own to scroll: codex reserves its input box
                // with a scroll region anchored at row 1, and rows
                // leaving the top of a region used to be dropped
                // rather than kept — so the pane's scrollback was
                // empty and the transcript key was the only way back.
                //
                // Grid::scroll_up_region now keeps them, which is what
                // iTerm2 does (measured 2026-09-07 with the identical
                // sequence: all 120 lines stayed reachable).  So the
                // wheel does what it does in every other pane, and
                // what it already did in claudecode — which is the
                // experience this was asked to match.  Ctrl+T is still
                // codex's own key for anyone who wants its transcript.
                // Re-issued every tick, for the same reason the agent-TUI
                // declaration above is: L2's state can outlive L1's.  A
                // one-shot clear kept in `self.declared` only fires for a
                // pane THIS L1 process declared for, so after an L1
                // restart — a silent update, or the cold launch after a
                // reboot — the set is empty, the clear is never sent, and
                // the wheel keys L2 is still holding stay held.  The
                // symptom is codex's Ctrl+T transcript opening on a wheel
                // scroll again, months after that was removed (2026-09-08
                // field report).  L2 drops a repeat clear silently, so the
                // heartbeat costs one small frame per tick per codex pane.
                let _ = host.set_pane_wheel_keys(sid, b"", b"", b"", b"");
                publish_badge(host, &mut self.last_badge, sid, &text);
            } else if self.last_badge.remove(&sid).is_some() {
                let _ = host.set_pane_agent_tui(sid, false);
                let _ = host.set_pane_wheel_keys(sid, b"", b"", b"", b"");
                // codex is gone; the pane is a shell again, and a
                // shell's `<u>` is somebody's text.
                let _ = host.set_pane_render_markup(sid, false);
                // codex left this pane: clear the badge we set, and
                // only the one we set — another plugin may own it now.
                let _ = host.set_pane_badge(sid, "");
            }
        }
    }
}


#[cfg(test)]
mod wheel_decl_tests {
    use super::*;

    /// The marker must be one the shared predicate can actually use —
    /// a blank or non-UTF-8 marker reads as "no marker", which would
    /// silently turn the wheel back into a blind toggle.
    #[test]
    fn the_declared_marker_is_usable() {
        assert_eq!(
            marspot::wheel_marker::needle(WHEEL_MARKER).as_deref(),
            Some("/TRANSCRIPT/")
        );
    }

    /// Pinned against a row captured off a live transcript: the
    /// declared marker must match the screen codex actually draws,
    /// which is letter-spaced.
    #[test]
    fn the_declared_marker_matches_the_real_screen() {
        let row = "/ T R A N S C R I P T / / / / / / / / / / / / / ";
        assert!(marspot::wheel_marker::shows_marker(
            row.chars().count() as u16,
            1,
            |col, _| row.chars().nth(col as usize).unwrap_or(' '),
            WHEEL_MARKER
        ));
    }

    /// Entering must not be confused with paging: if `enter` were one
    /// of the page keys, L2 could not both open and scroll.
    #[test]
    fn enter_is_distinct_from_the_page_keys() {
        assert_ne!(WHEEL_ENTER, WHEEL_UP);
        assert_ne!(WHEEL_ENTER, WHEEL_DOWN);
        assert_ne!(WHEEL_UP, WHEEL_DOWN);
    }

    /// The declaration names codex's own on-screen marker, so L2 can
    /// see whether the transcript is open instead of remembering that
    /// it opened one.  `Ctrl+T` toggles: a stale flag would close the
    /// view rather than open it.
    #[test]
    fn the_declaration_carries_a_marker_to_read_the_state_from() {
        // The bytes a wheel needs, as declared to the host.
        let (enter, up, down, marker): (&[u8], &[u8], &[u8], &[u8]) =
            (b"\x14", b"\x1b[5~", b"\x1b[6~", b"/TRANSCRIPT/");
        assert_eq!(enter, b"\x14", "Ctrl+T opens codex's transcript");
        assert_eq!(up, b"\x1b[5~", "PageUp");
        assert_eq!(down, b"\x1b[6~", "PageDown");
        assert!(!marker.is_empty(), "without a marker L2 would have to guess");
    }
}
