//! The files the worker keeps outside its own memory: registry records,
//! and the directories its sockets live in.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

pub(super) fn write_record(dirs: &[PathBuf], r: &Record) {
    for d in dirs {
        if let Err(e) = registry::write(d, r, now_ms()) {
            lx_warn!("peerbus.registry.write_failed", &e.to_string(), dir = d.display().to_string());
        }
    }
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
