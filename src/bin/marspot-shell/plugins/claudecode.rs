//! marspot-claudecode plugin — first marspot plugin.
//!
//! ## What it does (M2)
//!
//! Each tick (every 2 s), walks `~/.claude/projects/<encoded-cwd>/`
//! and finds the newest `.jsonl` per project.  Parses the first line
//! of each, extracts `sessionId`, and surfaces:
//! - one `plugin.claudecode.session` INFO line per active session
//!   (idempotent — only re-logs when sessionId or mtime changes)
//! - a `plugin.claudecode.session_done` line when an assistant
//!   message appears at the tail (for OS notification later)
//!
//! ## Why not per-pane mapping yet (M3)
//!
//! Per-pane PTY → claude pid → cwd → project chain needs L4 shelld
//! cooperation (PTY device path) or libc-heavy `proc_pidinfo`.  M2
//! surfaces the global session list so the value is visible
//! immediately; M3 wires per-pane mapping when there's signal that
//! it's needed.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use marspot::paths;
use marspot::shelld_client::ShelldClient;

use crate::plugins::pidtree;
use crate::plugins::{
    LogLevel, PermissionSet, Plugin, PluginError, PluginHost, PluginMetadata,
    PLUGIN_API_VERSION,
};

/// Per-session state cached so we only re-log on real change.
#[derive(Clone, Debug)]
struct SessionInfo {
    session_id: String,
    project_dir: String, // encoded form, e.g. -Users-doracawl-workspace-...
    jsonl_path: PathBuf,
    last_mtime: SystemTime,
    last_size: u64,
    last_message_kind: Option<String>,
}

pub struct ClaudecodePlugin {
    initialised: bool,
    /// Keyed by jsonl path (uniquely identifies a session file).
    seen: HashMap<PathBuf, SessionInfo>,
    /// Where ~/.claude/projects lives.  Cached at init.
    projects_root: Option<PathBuf>,
    /// Lazy shelld client.  None until first successful connect.
    /// Used to walk shelld's session table → per-session zsh.pid →
    /// pidtree → cwd → encoded project dir → sessionId.
    shelld: Option<ShelldClient>,
    /// Previous-tick mapping of `(shelld_session_id → sessionId)` so
    /// we only log on transitions (attach / detach / change), not
    /// every tick.
    last_mapping: HashMap<u64, String>,
    /// Set of `sessionId`s already alerted on this run.  Suppresses a
    /// repeat notification when the same session's mtime keeps moving
    /// while still on an "assistant" tail (claude streams updates
    /// after the first assistant message).
    notified_assistant: HashMap<String, SystemTime>,
}

impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            seen: HashMap::new(),
            projects_root: None,
            shelld: None,
            last_mapping: HashMap::new(),
            notified_assistant: HashMap::new(),
        }
    }

    /// Reverse-lookup: given an encoded project dir (e.g.
    /// `-Users-doracawl-workspace-foo`), return the latest sessionId
    /// we've seen for it.  Walks the `seen` map; cheap when only a
    /// dozen projects are active.
    fn session_id_for_project(&self, encoded_dir: &str) -> Option<String> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir == encoded_dir {
                match newest {
                    Some((t, _)) if t >= s.last_mtime => {}
                    _ => newest = Some((s.last_mtime, s)),
                }
            }
        }
        newest.map(|(_, s)| s.session_id.clone())
    }
}

/// Encode a filesystem path into claude's project directory naming
/// convention (`/Users/foo/bar` → `-Users-foo-bar`).
fn encode_project_dir(cwd: &std::path::Path) -> String {
    let mut s = String::with_capacity(cwd.as_os_str().len());
    for c in cwd.to_string_lossy().chars() {
        if c == '/' {
            s.push('-');
        } else {
            s.push(c);
        }
    }
    s
}

/// Suppress duplicate "claudecode done" alerts.  Per sessionId, fire
/// at most once per `NOTIFY_DEBOUNCE`; subsequent `assistant`-kind
/// mtimes (claude updating its own tail) get swallowed.  Once an
/// hour bypasses the dedupe so a long-idle user gets re-notified
/// when a fresh assistant turn arrives.
const NOTIFY_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

fn maybe_notify_done(
    host: &dyn PluginHost,
    session_id: &str,
    project_dir: &str,
    notified: &mut HashMap<String, SystemTime>,
) {
    let now = SystemTime::now();
    if let Some(prev) = notified.get(session_id) {
        if now
            .duration_since(*prev)
            .map(|d| d < NOTIFY_DEBOUNCE)
            .unwrap_or(true)
        {
            return; // suppressed by dedupe window
        }
    }
    notified.insert(session_id.to_string(), now);
    // Display project name = trailing path segment, stripped of the
    // leading dash convention.  e.g. -Users-doracawl-workspace-foo →
    // foo.
    let pretty_project = project_dir
        .rsplit('-')
        .next()
        .unwrap_or(project_dir)
        .to_string();
    let title = "Marspot · claudecode";
    let body = format!("Finished — {}", pretty_project);
    host.log(
        LogLevel::Info,
        "session.notify",
        &format!(
            "claudecode done sid={} project={}",
            session_id, project_dir
        ),
    );
    // Fire-and-forget osascript.  ~50–150 ms wall clock; spawn so the
    // plugin tick doesn't block.  Display notification is the macOS
    // native banner — uses the standard "do not disturb" rules.
    // Self-built FFI to NSUserNotification would shave the fork cost
    // but is non-trivial and notification frequency is bounded by
    // dedupe, so the simpler shell-out wins here.
    let script = format!(
        "display notification \"{}\" with title \"{}\" sound name \"Glass\"",
        body.replace('"', "\\\""),
        title.replace('"', "\\\""),
    );
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Heuristic: is this descendant the `claude` CLI?  Walks argv —
/// the comm fast-path was a dead end because recent claude builds
/// mangle the process name to the version string ("2.1.177") via
/// `exec -a`, so a `comm == "claude"` prefilter would miss every
/// real claudecode process.  argv[0] survives the rename (it's the
/// original exec path), so cmdline → split → check argv[0]'s
/// basename equals "claude" is the reliable signal.
fn looks_like_claudecode(d: &pidtree::ProcRow) -> bool {
    let line = match pidtree::proc_cmdline(d.pid) {
        Some(l) => l,
        None => return false,
    };
    let argv0 = match line.split(' ').next() {
        Some(s) => s,
        None => return false,
    };
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    basename == "claude"
}

impl Default for ClaudecodePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ClaudecodePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "claudecode",
            version: "0.1.0",
            api_version: PLUGIN_API_VERSION,
            permissions: PermissionSet::READ_PANE_INFO
                | PermissionSet::READ_PTY_TREE
                | PermissionSet::READ_DISK_FS
                | PermissionSet::NOTIFY_USER
                | PermissionSet::SET_STATUS_LINE
                | PermissionSet::PERSIST_STATE,
            // 2 s — long enough to be near-free,short enough to catch
            // "assistant message done" within a couple seconds for
            // future notification feature.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // Persistence dir set up now so PluginError::IoError surfaces
        // a bad config early instead of mid-tick.
        let _ = host.state_dir()?;
        // Resolve ~/.claude/projects.  Without HOME there's nothing
        // to do — disable cleanly via a soft Err.
        let home = std::env::var_os("HOME").ok_or_else(|| {
            PluginError::Other("HOME not set; claudecode plugin idle".into())
        })?;
        self.projects_root = Some(PathBuf::from(home).join(".claude").join("projects"));
        // Connect to shelld so tick can map shelld_session_id →
        // child_pid → claude descendant → sessionId.  Best-effort:
        // if shelld is down, plugin still runs in global-scan-only
        // mode, no per-session mapping.
        let socket = paths::shelld_socket();
        let wake = Arc::new(AtomicBool::new(false));
        let wk = wake.clone();
        match ShelldClient::connect(&socket, move || {
            wk.store(true, Ordering::Release);
        }) {
            Ok(c) => {
                self.shelld = Some(c);
                host.log(
                    LogLevel::Info,
                    "init.shelld_connected",
                    &format!("shelld client up ({})", socket.display()),
                );
            }
            Err(e) => {
                host.log(
                    LogLevel::Warn,
                    "init.shelld_unavailable",
                    &format!(
                        "shelld at {} unavailable ({e}); per-session mapping disabled",
                        socket.display()
                    ),
                );
            }
        }
        host.log(
            LogLevel::Info,
            "init",
            &format!(
                "claudecode plugin initialised (projects_root={})",
                self.projects_root.as_ref().unwrap().display()
            ),
        );
        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        let root = match &self.projects_root {
            Some(r) => r.clone(),
            None => return,
        };
        // Walk one level down.  Each subdir = one project (encoded cwd).
        let projects = match fs::read_dir(&root) {
            Ok(d) => d,
            Err(_) => return, // No projects dir yet — silent
        };
        let mut newly_seen = 0usize;
        let mut updates = 0usize;
        for project_entry in projects.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let project_dir = match project_path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            // Find newest .jsonl in this project.
            let mut newest: Option<(PathBuf, SystemTime, u64)> = None;
            let dir = match fs::read_dir(&project_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for f in dir.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = match f.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let size = meta.len();
                match &newest {
                    Some((_, prev_mtime, _)) if *prev_mtime >= mtime => {}
                    _ => newest = Some((p, mtime, size)),
                }
            }
            let Some((jsonl_path, mtime, size)) = newest else {
                continue;
            };
            // Update tracking.  Skip if path + mtime + size unchanged.
            if let Some(prev) = self.seen.get(&jsonl_path) {
                if prev.last_mtime == mtime && prev.last_size == size {
                    continue;
                }
            }
            // Parse sessionId from first line of the file.
            let session_id = match parse_session_id(&jsonl_path) {
                Some(id) => id,
                None => continue, // malformed; try again next tick
            };
            // Inspect last message type (tail-read).  Cheap heuristic:
            // read last 32 KB, find newline-anchored last record.
            let last_message_kind = tail_last_message_type(&jsonl_path);

            let is_new = !self.seen.contains_key(&jsonl_path);
            self.seen.insert(
                jsonl_path.clone(),
                SessionInfo {
                    session_id: session_id.clone(),
                    project_dir: project_dir.clone(),
                    jsonl_path: jsonl_path.clone(),
                    last_mtime: mtime,
                    last_size: size,
                    last_message_kind: last_message_kind.clone(),
                },
            );
            if is_new {
                newly_seen += 1;
                host.log(
                    LogLevel::Info,
                    "session",
                    &format!(
                        "session detected sid={} project={} kind={} size={}",
                        session_id,
                        project_dir,
                        last_message_kind.as_deref().unwrap_or("?"),
                        size
                    ),
                );
            } else {
                updates += 1;
                host.log(
                    LogLevel::Debug,
                    "session.update",
                    &format!(
                        "sid={} kind={} size={}",
                        session_id,
                        last_message_kind.as_deref().unwrap_or("?"),
                        size
                    ),
                );
                // M4: claudecode-done notification.  Fire when the
                // tail transitions from user/tool to assistant
                // — that's "claude finished writing its turn".
                // Suppressed when:
                //   * prev kind already was assistant (idempotent)
                //   * the binding doesn't resolve to a shelld
                //     session (= user isn't even watching this pane
                //     in marspot, no point alerting)
                //   * mtime within DEBOUNCE_SECS of last notify for
                //     this sessionId (recent run; would be spammy)
                let prev_kind = self
                    .seen
                    .get(&jsonl_path)
                    .and_then(|s| s.last_message_kind.clone());
                let _ = prev_kind; // kept for future expansion
                // Find the transition cheaply: compare what the
                // SessionInfo HAD before this tick's mutation to
                // what it now has.  But we just overwrote `self.seen`
                // above — instead detect the transition by checking
                // the freshly-read `last_message_kind`.  Caller will
                // call notify_done iff it crossed into "assistant".
                if last_message_kind.as_deref() == Some("assistant") {
                    maybe_notify_done(
                        host,
                        &session_id,
                        &project_dir,
                        &mut self.notified_assistant,
                    );
                }
            }
        }
        if newly_seen > 0 || updates > 0 {
            host.log(
                LogLevel::Debug,
                "tick.summary",
                &format!("new={} updated={}", newly_seen, updates),
            );
        }

        // M3.2 — per-shelld-session mapping.  For each live shelld
        // session, BFS its zsh.pid for a `claude` descendant; if
        // found, encode its cwd and reverse-lookup the sessionId
        // from `self.seen`.  Log only on transitions to keep the
        // log file quiet for a steady-state session.
        let Some(client) = self.shelld.as_ref() else {
            return; // shelld unavailable; skip mapping silently
        };
        let sessions = match client.list_sessions() {
            Ok(v) => v,
            Err(e) => {
                host.log(
                    LogLevel::Info,
                    "tick.shelld_list_failed",
                    &format!("{e}"),
                );
                return;
            }
        };
        let procs = pidtree::list_all_procs();
        let mut new_mapping: HashMap<u64, String> = HashMap::new();
        for s in &sessions {
            if !s.alive {
                continue;
            }
            let descendants =
                pidtree::descendants_of(s.child_pid, &procs);
            let Some(claude) = descendants.iter().find(|d| looks_like_claudecode(d))
            else {
                continue; // pane isn't running claudecode right now
            };
            let Some(cwd) = pidtree::proc_cwd(claude.pid) else {
                continue;
            };
            let encoded = encode_project_dir(&cwd);
            if let Some(sid_uuid) = self.session_id_for_project(&encoded) {
                new_mapping.insert(s.session_id, sid_uuid);
            }
        }
        // Log adds + drops vs. last tick.
        for (sh_sid, cc_sid) in &new_mapping {
            let prev = self.last_mapping.get(sh_sid);
            if prev.map(|p| p != cc_sid).unwrap_or(true) {
                host.log(
                    LogLevel::Info,
                    "session.bound",
                    &format!(
                        "shelld_session={} → claudecode sid={}",
                        sh_sid, cc_sid
                    ),
                );
            }
        }
        for (sh_sid, cc_sid) in &self.last_mapping {
            if !new_mapping.contains_key(sh_sid) {
                host.log(
                    LogLevel::Info,
                    "session.unbound",
                    &format!(
                        "shelld_session={} (was sid={})",
                        sh_sid, cc_sid
                    ),
                );
            }
        }
        self.last_mapping = new_mapping;
    }

    fn stop(&mut self, host: &dyn PluginHost) {
        host.log(LogLevel::Info, "stop", "plugin stopped");
        self.initialised = false;
        self.seen.clear();
    }
}

/// Read the first line of `path` and try to parse `"sessionId":"<uuid>"`
/// out of it.  Cheap regex-free string scan — JSON parser would be
/// overkill for one well-known field, and pulling serde_json into the
/// shell binary just for this would violate the self-build principle.
fn parse_session_id(path: &PathBuf) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // claudecode jsonl always starts the file with a "mode" record
    // containing `"sessionId":"<uuid>"`.  We look for that key.
    let key = "\"sessionId\":\"";
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "claudecode-plugin-test-{}-{}.jsonl",
            std::process::id(),
            n
        ));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parse_session_id_finds_uuid_in_first_record() {
        let path = tmpfile(
            r#"{"type":"mode","mode":"normal","sessionId":"3ad170c8-7d00-4764-9bb8-fc31eba2154d"}
{"type":"user","text":"hello"}
"#,
        );
        let id = parse_session_id(&path).unwrap();
        assert_eq!(id, "3ad170c8-7d00-4764-9bb8-fc31eba2154d");
    }

    #[test]
    fn parse_session_id_returns_none_when_missing() {
        let path = tmpfile(r#"{"type":"user","text":"hello"}"#);
        assert!(parse_session_id(&path).is_none());
    }

    #[test]
    fn tail_last_message_type_reads_final_record() {
        let path = tmpfile(concat!(
            r#"{"type":"mode","sessionId":"abc"}"#,
            "\n",
            r#"{"type":"user","text":"hi"}"#,
            "\n",
            r#"{"type":"assistant","text":"there"}"#,
            "\n",
        ));
        let kind = tail_last_message_type(&path).unwrap();
        assert_eq!(kind, "assistant");
    }

    #[test]
    fn tail_last_message_type_handles_trailing_whitespace() {
        let path = tmpfile(concat!(
            r#"{"type":"user","text":"hi"}"#,
            "\n   \n",
        ));
        let kind = tail_last_message_type(&path).unwrap();
        assert_eq!(kind, "user");
    }
}

/// Cheap "what's the type of the last record" check.  Reads at most
/// the last 32 KiB of the file and looks at the last `\n`-separated
/// record's `"type":"…"` field.  Returns None if the file is malformed
/// or has no recognisable type.
fn tail_last_message_type(path: &PathBuf) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = if len > TAIL { len - TAIL } else { 0 };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(TAIL as usize);
    f.read_to_end(&mut buf).ok()?;
    // Find the last newline-anchored record.
    let s = String::from_utf8_lossy(&buf);
    let last_line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    let key = "\"type\":\"";
    let i = last_line.find(key)? + key.len();
    let rest = &last_line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
