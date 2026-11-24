//! The files the worker keeps outside its own memory: registry records,
//! and the directories its sockets live in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use marspot::pidtree::ProcId;
use marspot_peer::registry::{self, Record};
use marspot_term::lx_warn;

use super::worker::Worker;

impl Worker {
    /// Claude Code checks that a reply address is in the same directory
    /// as its own socket, so the inbox goes where live Claude sessions
    /// put theirs.
    pub(super) fn sock_dir(&mut self) -> std::io::Result<PathBuf> {
        if let Some(d) = &self.sock_dir {
            return Ok(d.clone());
        }
        let from_claude = registry::default_dirs()
            .iter()
            .flat_map(|d| registry::list_live(d))
            .filter(|e| !e.ours)
            .find_map(|e| e.socket.and_then(|s| Path::new(&s).parent().map(Path::to_path_buf)));
        let dir = from_claude.unwrap_or_else(|| PathBuf::from("/tmp/cc-socks"));
        if !dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
        }
        self.sock_dir = Some(dir.clone());
        Ok(dir)
    }
}

/// The pid each record of ours is filed under (its relay's) and when it
/// was made, by the pid of the agent it stands for: one per binding.
static FILED: Mutex<Option<HashMap<i32, (i32, i64)>>> = Mutex::new(None);

pub(super) fn write_record(dirs: &[PathBuf], r: &Record) {
    for d in dirs {
        if let Err(e) = registry::write(d, r, now_ms()) {
            lx_warn!("peerbus.registry.write_failed", &e.to_string(), dir = d.display().to_string());
        }
    }
    FILED.lock().unwrap().get_or_insert_default().insert(r.agent_pid, (r.pid, r.started_at_ms));
    crate::plugins::helpers::record_written();
}

/// Remove record `r` from every registry directory.
pub(super) fn remove_record(r: &Record) {
    for d in registry::default_dirs() {
        let _ = registry::remove_ours(&d, r.pid);
    }
    unfile(r);
}

/// Forget where record `r` is filed.
pub(super) fn unfile(r: &Record) {
    if let Some(m) = FILED.lock().unwrap().as_mut() {
        m.retain(|agent, (relay, _)| (*agent, *relay) != (r.agent_pid, r.pid));
    }
}

/// The pid the record marspot keeps for `agent` is filed under, while it
/// keeps one.  One made before `agent` started stands for an earlier
/// process its pid was reused from, whose binding is not undone yet.
pub(super) fn filed_under(agent: ProcId) -> Option<i32> {
    let (relay, made_ms) = *FILED.lock().unwrap().as_ref()?.get(&agent.pid)?;
    (made_ms >= (agent.start_us / 1000) as i64).then_some(relay)
}

/// `<pid>-<8 hex>.sock`: only files of this shape are ours to delete.
pub(super) fn is_inbox_name(path: &str) -> bool {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else { return false };
    let Some((pid, rest)) = name.strip_suffix(".sock").and_then(|r| r.split_once('-')) else { return false };
    !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && rest.len() == 8 && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Where this marspot keeps the sockets its relays talk to it on.
pub(super) fn private_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = marspot_term::paths::state_root().join("peerbus");
    std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_inbox_shaped_names_are_deleted() {
        assert!(is_inbox_name("/tmp/cc-socks/42-0a1b2c3d.sock"));
        assert!(!is_inbox_name("/tmp/cc-socks/42.sock"));
        assert!(!is_inbox_name("/tmp/cc-socks/x42-0a1b2c3d.sock"));
        assert!(!is_inbox_name("/tmp/cc-socks/42-0a1b2c3dx.sock"));
    }
}
