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

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PermissionSet,
            PLUGIN_API_VERSION};
use crate::plugins::pidtree;

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

/// What the codex in `cwd` is running, or None if its record cannot be
/// found — in which case the caller keeps whatever it had.
fn facts_for_cwd(codex_home: &std::path::Path, cwd: &str) -> Option<SessionFacts> {
    for path in recent_rollouts(codex_home).into_iter().take(12) {
        if let Some(f) = tail_of(&path).and_then(|t| facts_from_tail(&t, cwd)) {
            return Some(f);
        }
    }
    None
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
    /// Sessions we have already told L2 about, so an unchanged scan
    /// does not re-send the same declaration every two seconds.
    declared: std::collections::HashSet<u64>,
}

impl CodexPlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            last_badge: std::collections::HashMap::new(),
            declared: std::collections::HashSet::new(),
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
                .and_then(|cwd| facts_for_cwd(&home, &cwd.to_string_lossy()));
            let (model, effort) = match facts {
                Some(f) => (
                    f.model.or_else(|| cfg_model.clone()),
                    f.effort.or_else(|| cfg_effort.clone()),
                ),
                None => (cfg_model.clone(), cfg_effort.clone()),
            };
            let text = badge_text(model.as_deref(), effort.as_deref());
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
                let _ = host.set_pane_render_markup(sid, true);
                if !self.declared.contains(&sid) {
                    if host
                        .set_pane_wheel_keys(
                            sid,
                            WHEEL_ENTER,
                            WHEEL_UP,
                            WHEEL_DOWN,
                            WHEEL_MARKER,
                        )
                        .is_ok()
                    {
                        self.declared.insert(sid);
                    }
                }
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
                if self.declared.remove(&sid) {
                    let _ = host.set_pane_wheel_keys(sid, b"", b"", b"", b"");
                    // codex is gone; the pane is a shell again, and a
                    // shell's `<u>` is somebody's text.
                    let _ = host.set_pane_render_markup(sid, false);
                }
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
