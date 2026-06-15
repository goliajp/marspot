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
use std::time::SystemTime;

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
}

impl ClaudecodePlugin {
    pub fn new() -> Self {
        Self {
            initialised: false,
            seen: HashMap::new(),
            projects_root: None,
        }
    }
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
                // M3: when last_message_kind transitions to
                // "assistant" → emit a NOTIFY for "claudecode done".
            }
        }
        if newly_seen > 0 || updates > 0 {
            host.log(
                LogLevel::Debug,
                "tick.summary",
                &format!("new={} updated={}", newly_seen, updates),
            );
        }
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
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "claudecode-plugin-test-{}.jsonl",
            std::process::id()
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
