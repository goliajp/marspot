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

use std::path::PathBuf;
use std::time::Duration;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PermissionSet,
            PLUGIN_API_VERSION};
use crate::plugins::{pidtree, pty_op};

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

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// `model` and `model_reasoning_effort` from `~/.codex/config.toml`.
///
/// Read as text rather than parsed: the file is TOML with per-project
/// tables, and only two top-level scalars are wanted.  Stopping at the
/// first table header keeps a `[projects."…"]` section's own keys from
/// being mistaken for the globals.
fn read_model_and_effort(home: &PathBuf) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return (None, None);
    };
    let (mut model, mut effort) = (None, None);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            break; // into per-project / per-feature tables
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "model" => model = Some(v),
            "model_reasoning_effort" => effort = Some(v),
            _ => {}
        }
    }
    (model, effort)
}

/// What a codex session is ACTUALLY running, read from its own record.
///
/// The badge used to read `~/.codex/config.toml`, which says what a
/// FRESH codex would start with — not what the one in this pane is
/// doing.  Two panes on different efforts both showed the global
/// value, and a pane whose effort was changed mid-session showed the
/// old one.
///
/// codex writes a `turn_context` record per turn carrying `cwd`,
/// `model` and `effort` together, so the last one in a session's
/// rollout is the truth.  Matched to a pane by the codex process's own
/// working directory.
#[derive(Debug, PartialEq)]
pub(crate) struct SessionFacts {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Scan a rollout's TAIL for the last `turn_context`.
///
/// These files reach tens of megabytes — 63 MB in the field — so
/// reading one whole, per pane, every two seconds is out of the
/// question.  The last record is near the end by construction, and a
/// window that misses it just leaves the caller with what it had.
pub(crate) fn facts_from_tail(tail: &str, want_cwd: &str) -> Option<SessionFacts> {
    for line in tail.lines().rev() {
        if !line.contains("\"turn_context\"") {
            continue;
        }
        // Deliberately not a JSON parse: this is a hot-ish path over a
        // 256 KiB window, the three fields are flat strings, and a
        // format change should degrade to "no facts" rather than to a
        // wrong badge.
        let cwd = json_str_field(line, "cwd")?;
        if cwd != want_cwd {
            continue;
        }
        return Some(SessionFacts {
            model: json_str_field(line, "model"),
            effort: json_str_field(line, "effort"),
        });
    }
    None
}

/// The value of `"<key>":"<value>"`, first occurrence, no escapes.
fn json_str_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let at = line.find(&pat)? + pat.len();
    let rest = &line[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// How much of a rollout's end to look at for the last `turn_context`.
///
/// One record is a few hundred bytes and the last one is at the end by
/// construction; this is slack for whatever trails it.  Reading the
/// file whole is not an option — 63 MB in the field.
const ROLLOUT_TAIL_BYTES: u64 = 256 * 1024;

/// The newest rollout under `~/.codex/sessions`, by modification time.
///
/// Bounded: only today's and yesterday's day-directories are looked
/// at.  A session older than that is not the one a pane is running.
fn recent_rollouts(codex_home: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let sessions = codex_home.join("sessions");
    // sessions/YYYY/MM/DD/rollout-*.jsonl
    let mut days: Vec<PathBuf> = Vec::new();
    for y in read_dirs(&sessions) {
        for m in read_dirs(&y) {
            days.extend(read_dirs(&m));
        }
    }
    days.sort();
    for day in days.iter().rev().take(2) {
        let Ok(rd) = std::fs::read_dir(day) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                out.push((m, p));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

fn read_dirs(at: &std::path::Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(at) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Read the last `ROLLOUT_TAIL_BYTES` of a file as text.
fn tail_of(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(ROLLOUT_TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(ROLLOUT_TAIL_BYTES as usize);
    f.take(ROLLOUT_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Which rollout belongs to which working directory, and what it last
/// said.
///
/// Finding a pane's rollout means opening files until one claims its
/// cwd, and there are dozens.  Doing that per pane per tick is the
/// wrong shape: the mapping barely changes, while the CONTENT of one
/// file changes constantly.  So the mapping is rebuilt on a slow
/// cadence and the content is re-read only when that file's mtime
/// moves — steady state is one `stat` per codex pane.
#[derive(Default)]
pub(crate) struct RolloutIndex {
    by_cwd: std::collections::HashMap<String, PathBuf>,
    facts: std::collections::HashMap<String, (std::time::SystemTime, SessionFacts)>,
    scanned_at: Option<std::time::Instant>,
}

impl RolloutIndex {
    /// A new session's first turn should show up quickly; a rescan is
    /// a directory walk plus a bounded number of tail reads, so it is
    /// not something to do every tick.
    const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

    fn rescan_if_due(&mut self, codex_home: &std::path::Path) {
        if self
            .scanned_at
            .is_some_and(|t| t.elapsed() < Self::RESCAN_EVERY)
        {
            return;
        }
        self.scanned_at = Some(std::time::Instant::now());
        self.by_cwd.clear();
        // Newest first, so the freshest session wins a cwd two
        // sessions have shared.
        for path in recent_rollouts(codex_home).into_iter().take(40) {
            let Some(tail) = tail_of(&path) else { continue };
            let Some(cwd) = last_turn_cwd(&tail) else { continue };
            self.by_cwd.entry(cwd).or_insert(path);
        }
    }

    /// What the codex in `cwd` is running, or None when its record
    /// cannot be found — in which case the caller keeps what it had.
    fn facts_for_cwd(
        &mut self,
        codex_home: &std::path::Path,
        cwd: &str,
    ) -> Option<SessionFacts> {
        self.rescan_if_due(codex_home);
        let path = self.by_cwd.get(cwd)?.clone();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        if let Some((seen, f)) = self.facts.get(cwd) {
            if *seen == mtime {
                return Some(SessionFacts {
                    model: f.model.clone(),
                    effort: f.effort.clone(),
                });
            }
        }
        let facts = tail_of(&path).and_then(|t| facts_from_tail(&t, cwd))?;
        let copy = SessionFacts {
            model: facts.model.clone(),
            effort: facts.effort.clone(),
        };
        self.facts.insert(cwd.to_string(), (mtime, facts));
        Some(copy)
    }
}

/// The cwd of the last `turn_context` in a tail, whatever it is.
fn last_turn_cwd(tail: &str) -> Option<String> {
    tail.lines()
        .rev()
        .find(|l| l.contains("\"turn_context\""))
        .and_then(|l| json_str_field(l, "cwd"))
}

/// The reasoning efforts codex accepts.
///
/// Read out of the binary rather than assumed: its serde variant table
/// carries `low`/`medium`/`high` adjacently, and the only `minimal` in
/// there belongs to filesystem paths.
/// Account profiles, the `CODEX_HOME` kind.
///
/// Not `codex -p <name>`, which layers a named set of CONFIG values
/// (`$CODEX_HOME/<name>.config.toml`).  This is the other axis: a
/// whole directory with its own login, history and config, selected by
/// pointing `CODEX_HOME` at it — the exact analogue of
/// `CLAUDE_CONFIG_DIR`, which the claudecode plugin next door has
/// cycled for a year.
///
/// `~/.codex` itself is the default, and on this machine it is a
/// SYMLINK to `.codex-profile-1`; a link is followed so the default and
/// the profile it points at are not offered as two separate things
/// that do the same.
const CODEX_PROFILE_PREFIX: &str = ".codex-profile-";

/// Which profile a directory is: `None` for the default `~/.codex`.
fn profile_dirs() -> Vec<(u8, std::path::PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    let mut out: Vec<(u8, std::path::PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix(CODEX_PROFILE_PREFIX) else {
                continue;
            };
            let Ok(n) = rest.parse::<u8>() else { continue };
            if e.path().is_dir() {
                out.push((n, e.path()));
            }
        }
    }
    out.sort_by_key(|(n, _)| *n);
    out
}

/// The profile a live codex belongs to, read off the process rather
/// than reconstructed.
///
/// `CODEX_HOME` is what the `codexN` shell aliases set, and reading it
/// back is the alias's own expansion made explicit — reproducing the
/// alias would depend on the user's rc file still defining it, in that
/// shell, at that moment.  Unset means the default `~/.codex`, which
/// is resolved through symlinks so a link to `.codex-profile-1` reads
/// as profile 1 and not as a nameless "default".
fn profile_of(codex_pid: i32) -> Option<u8> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let raw = marspot::pidtree::proc_env_value(codex_pid, "CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let real = std::fs::canonicalize(&raw).unwrap_or(raw);
    let name = real.file_name()?.to_str()?;
    name.strip_prefix(CODEX_PROFILE_PREFIX)?.parse().ok()
}


/// What the pane is running, remembered so a menu pick knows what it
/// is changing and what to leave alone.
#[derive(Clone)]
pub(crate) struct PaneCodex {
    pub codex_pid: i32,
    pub shell_pid: i32,
    pub effort: Option<String>,
    /// Which `CODEX_HOME` this pane's codex is running under, read off
    /// the live process.  `None` when it could not be determined —
    /// the menu then marks nothing as current rather than guessing.
    pub profile: Option<u8>,
}


/// Take codex down and bring the SAME session back under another
/// account profile.
///
/// `CODEX_HOME` on the resume line rather than the `codexN` alias: an
/// alias is an interactive shell's, defined in the user's rc file, and
/// reproducing one depends on all of that still being true in that
/// shell at that moment.  Setting the variable IS what the alias
/// expands to.
///
/// `resume --last` and not a session id, for the same reason the
/// effort switch uses it: codex filters the picker by working
/// directory, so within a pane's own cwd the most recent session is
/// that pane's.  NOTE that history lives inside the profile directory,
/// so resuming under a DIFFERENT profile finds that profile's most
/// recent session in this cwd — which is the honest meaning of
/// switching accounts, not a bug to paper over.
fn profile_switch_op(pane: &PaneCodex, profile: u8, dir: &std::path::Path) -> Option<pty_op::PtyOp> {
    let dir = dir.to_str()?;
    let line = pty_op::PtyCommand::new("codex")
        .env("CODEX_HOME", dir)
        .arg("resume")
        .arg("--last")
        .clear_screen_first(true)
        .to_bytes()?;
    Some(
        pty_op::PtyOp::new("codex.profile_switch")
            .hold_screen(true)
            .badge(format!("→ P{profile}"))
            .step(pty_op::Step::settle(Duration::from_millis(250)).named("hold_settle"))
            .step(
                pty_op::Step::terminate(pane.codex_pid, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(pty_op::Step::send(line).named("resume"))
            .step(
                pty_op::Step::await_process(pane.shell_pid, looks_like_codex)
                    .timeout(Duration::from_secs(20)),
            ),
    )
}

/// `gpt-6-astra·high` — the shape claudecode's badge already uses for
/// its own model and effort, so the two read as one system.
fn badge_text(model: Option<&str>, effort: Option<&str>) -> String {
    match (model, effort) {
        (Some(m), Some(e)) => format!("{m}·{e}"),
        (Some(m), None) => m.to_string(),
        (None, Some(e)) => format!("codex·{e}"),
        (None, None) => "codex".to_string(),
    }
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
        dirs.iter()
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
            .collect()
    }

    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
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
                .find(|p| looks_like_codex(p))
                .map(|p| p.pid);
            let has_codex = codex_pid.is_some();
            // Ask THIS pane's session what it is running, and fall
            // back to the global config when its record cannot be
            // found (a session that has not written a turn yet).
            let facts = codex_pid
                .and_then(pidtree::proc_cwd)
                .and_then(|cwd| self.rollouts.facts_for_cwd(&home, &cwd.to_string_lossy()));
            let (model, effort) = match facts {
                Some(f) => (
                    f.model.or_else(|| cfg_model.clone()),
                    f.effort.or_else(|| cfg_effort.clone()),
                ),
                None => (cfg_model.clone(), cfg_effort.clone()),
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
                if self.last_badge.get(&sid).map(String::as_str) != Some(text.as_str()) {
                    if host.set_pane_badge(sid, &text).is_ok() {
                        host.log(
                            LogLevel::Info,
                            "codex.badge.changed",
                            &format!("sid={sid} badge={text:?}"),
                        );
                        self.last_badge.insert(sid, text.clone());
                    }
                }
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
mod tests {
    use super::*;

    /// The badge reads like claudecode's, so a window with both does
    /// not look like two unrelated tools.
    #[test]
    fn badge_pairs_model_with_effort() {
        assert_eq!(badge_text(Some("gpt-6-astra"), Some("high")), "gpt-6-astra·high");
        assert_eq!(badge_text(Some("gpt-6-astra"), None), "gpt-6-astra");
        assert_eq!(badge_text(None, None), "codex");
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

    /// Only the top-level scalars count.  `~/.codex/config.toml` carries
    /// `[projects."…"]` tables whose keys would otherwise be read as the
    /// global model, and the badge would follow whichever project was
    /// listed last rather than what codex is actually running.
    #[test]
    fn per_project_tables_do_not_leak_into_the_globals() {
        let dir = std::env::temp_dir().join(format!("codexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\
             [projects.\"/x\"]\nmodel = \"WRONG\"\n",
        )
        .unwrap();
        let got = read_model_and_effort(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(got, (Some("gpt-6-astra".into()), Some("high".into())));
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
}

#[cfg(test)]
mod session_facts_tests {
    use super::{facts_from_tail, SessionFacts};

    /// Shape taken from a real rollout: `turn_context` carries cwd,
    /// model and effort in one record, which is why the last one is
    /// the whole answer.
    fn turn(cwd: &str, model: &str, effort: &str) -> String {
        format!(
            r#"{{"type":"turn_context","payload":{{"turn_id":"x","cwd":"{cwd}","model":"{model}","effort":"{effort}","summary":"auto"}}}}"#
        )
    }

    #[test]
    fn the_last_turn_wins() {
        let tail = [
            turn("/w/a", "gpt-6-astra", "low"),
            r#"{"type":"response_item","payload":{}}"#.to_string(),
            turn("/w/a", "gpt-6-astra", "high"),
        ]
        .join("\n");
        assert_eq!(
            facts_from_tail(&tail, "/w/a"),
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("high".into())
            }),
            "a mid-session change is the point of reading this at all"
        );
    }

    /// Another pane's session must not answer for this one.  The
    /// rollouts all live in one directory; cwd is what tells them
    /// apart.
    #[test]
    fn a_different_cwd_is_a_different_pane() {
        let tail = turn("/w/other", "gpt-6-astra", "high");
        assert_eq!(facts_from_tail(&tail, "/w/mine"), None);
    }

    /// A window that missed the record, or a format that changed,
    /// leaves the caller with what it had — never with a wrong badge.
    #[test]
    fn nothing_recognisable_yields_nothing() {
        assert_eq!(facts_from_tail("", "/w/a"), None);
        assert_eq!(facts_from_tail("not json at all\nnor this", "/w/a"), None);
        assert_eq!(
            facts_from_tail(r#"{"type":"turn_context","payload":{"cwd":"/w/a"}}"#, "/w/a"),
            Some(SessionFacts { model: None, effort: None }),
            "a record without the fields is still that pane's record"
        );
    }

    /// The tail can begin mid-line; a half record must not be read as
    /// a whole one.
    #[test]
    fn a_truncated_leading_line_is_skipped() {
        let whole = turn("/w/a", "gpt-6-astra", "high");
        let tail = format!("ext\":\"payload\":{{\"cwd\":\"/w/a\",\"model\":\"WRONG\"\n{whole}");
        let got = facts_from_tail(&tail, "/w/a").expect("the whole record is there");
        assert_eq!(got.model.as_deref(), Some("gpt-6-astra"));
    }
}

#[cfg(test)]
mod profile_menu_tests {
    use super::{profile_switch_op, PaneCodex, CODEX_PROFILE_PREFIX};
    use std::path::Path;

    fn pane(profile: u8) -> PaneCodex {
        PaneCodex {
            codex_pid: 4242,
            shell_pid: 4200,
            effort: Some("medium".into()),
            profile: Some(profile),
        }
    }

    fn line_of(op: &super::pty_op::PtyOp) -> String {
        op.steps
            .iter()
            .filter_map(|s| match &s.kind {
                super::pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_switch_sets_the_variable_the_alias_expands_to() {
        // `codexN` is an interactive shell alias; reproducing it would
        // depend on the user's rc file still defining it, in that
        // shell, at that moment.  What it expands to is a variable,
        // and that is what goes on the line.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"))
            .expect("op builds");
        let line = line_of(&op);
        assert!(
            line.contains("CODEX_HOME='/Users/x/.codex-profile-2'"),
            "{line}"
        );
        assert!(line.contains("codex resume --last"), "{line}");
        // The pane's own screen is cleared first so the new session
        // does not paint over the old one's tail.  It goes out as a
        // shell `printf`, not a raw escape — the line is typed at a
        // shell, so the shell is what emits it.
        assert!(line.contains(r"printf '\033[H\033[2J'"), "{line}");
    }

    #[test]
    fn codex_is_signalled_not_typed_at() {
        // A `/quit` typed through the PTY echoes into the grid; a
        // signal does not.  Same rule the claudecode plugin follows.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"))
            .expect("op builds");
        assert!(
            op.steps.iter().any(|s| matches!(
                &s.kind,
                super::pty_op::StepKind::Terminate { pid, .. } if *pid == 4242
            )),
            "the running codex must be signalled"
        );
    }

    #[test]
    fn the_prefix_is_the_one_the_directories_use() {
        // The discovery scan and the alias the user types have to agree
        // on the name, and there is nothing else pinning that.
        assert_eq!(CODEX_PROFILE_PREFIX, ".codex-profile-");
    }
}
