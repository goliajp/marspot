//! The MCP config every claude marspot starts itself is given on its
//! command line (`--mcp-config`): one file in the state root naming
//! the servers of marspot's table, `marspot-peer mcp` and, while it is
//! installed, `madcawl-mcp`.  Brought up to date at each such start
//! and on every config pass, so a server installed or removed since
//! is in or out of the next claude started.
//!
//! The accounts' own `.claude.json` files are left alone.  Claude Code
//! rewrites them continually while a session runs, one per account,
//! and a second writer would race it for the same file.

use std::path::{Path, PathBuf};

use marspot_peer::json::obj;
use marspot_term::lx_warn;

use super::claude_hooks::pretty;
use super::mcp_servers::{Servers, json_reconcile, servers};
use crate::plugins::pty_op::{PtyCommand, quotable};

const FILE: &str = "claude-mcp.json";

/// `cmd` with `--mcp-config <state root>/claude-mcp.json` added, the
/// file brought up to date first.  Unchanged when there is no
/// `marspot-peer` to point at or the file cannot be written: claude
/// then starts as it did before, without the pane tools.
pub(crate) fn with_config(cmd: PtyCommand) -> PtyCommand {
    match write() {
        Ok(Some((path, _))) => add(cmd, &path),
        Ok(None) => cmd,
        Err((path, e)) => {
            lx_warn!("peerbus.claude_mcp.write_failed", &e.to_string(), path = path.display().to_string());
            cmd
        }
    }
}

/// The file brought up to date on a config pass.  Returns lines worth
/// logging.
pub(super) fn reconcile() -> Vec<String> {
    match write() {
        Ok(Some((path, true))) => vec![format!("{}: MCP servers for claude written", path.display())],
        Ok(_) => Vec::new(),
        Err((path, e)) => vec![format!("{}: {e}", path.display())],
    }
}

/// The file and whether it had to be written; None when there is no
/// `marspot-peer` to point at.
fn write() -> Result<Option<(PathBuf, bool)>, (PathBuf, std::io::Error)> {
    let Some(bin) = super::inbox::peer_bin() else { return Ok(None) };
    let Some(bin) = bin.to_str() else { return Ok(None) };
    let path = marspot_term::paths::state_root().join(FILE);
    match write_if_changed(&path, &text(&servers(bin))) {
        Ok(written) => Ok(Some((path, written))),
        Err(e) => Err((path, e)),
    }
}

/// The flag and its value, next to each other.  `--mcp-config` takes
/// several values, so whatever follows it on the line must be another
/// flag or `--`, never a bare word.
fn add(cmd: PtyCommand, path: &Path) -> PtyCommand {
    match path.to_str().filter(|p| quotable(p)) {
        Some(p) => cmd.arg("--mcp-config").quoted_arg(p),
        None => cmd,
    }
}

pub(super) fn text(table: &Servers) -> String {
    let mut servers = obj([]);
    json_reconcile(&mut servers, table);
    let mut s = pretty(&obj([("mcpServers", servers)]));
    s.push('\n');
    s
}

/// True when the file had to be written.  Renamed into place, so a
/// claude starting meanwhile reads the old text or the new, never half.
fn write_if_changed(path: &Path, text: &str) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|have| have == text.as_bytes()) {
        return Ok(false);
    }
    let dir = path.parent().expect("a file in the state root");
    std::fs::create_dir_all(dir)?;
    let tmp: PathBuf = dir.join(format!(".{FILE}.{}.tmp", marspot_peer::rand::hex(6)));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json::Value;
    use marspot_term::test_env::{EnvGuard, TempDir};

    /// What a switched-to claude is started with: the flag, its file
    /// in the state root, and the file naming the helper.  Written
    /// once, left alone while nothing changed, repointed when the
    /// helper moves.
    #[test]
    fn switch_mcp_config_is_written_once_and_follows_the_helper() {
        let state = TempDir::new("claude-mcp");
        let mut env = EnvGuard::new();
        env.set("MARSPOT_STATE_DIR", state.path()).set("MARSPOT_PEER_BIN", "/a/marspot-peer").set("HOME", state.path());
        let path = state.join(FILE);
        let line = String::from_utf8(with_config(PtyCommand::new("claude")).to_bytes().unwrap()).unwrap();
        assert_eq!(line, format!("claude --mcp-config '{}'\r", path.display()));
        let v = marspot_peer::json::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v.at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some("/a/marspot-peer"));
        assert_eq!(v.at(&["mcpServers", "marspot", "args"]), Some(&Value::Arr(vec![Value::from("mcp")])));

        assert!(!write_if_changed(&path, &text(&super::super::mcp_servers::of("/a/marspot-peer", None))).unwrap(), "same text, no write");
        env.set("MARSPOT_PEER_BIN", "/b/marspot-peer");
        with_config(PtyCommand::new("claude"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"/b/marspot-peer\""));
        // nothing of the write is left beside it
        assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 1);
    }

    /// A state root a single quote cannot be put around adds nothing
    /// rather than breaking the line that starts claude.
    #[test]
    fn switch_mcp_config_that_cannot_be_quoted_is_left_off() {
        let line = add(PtyCommand::new("claude"), Path::new("/tmp/it's/claude-mcp.json")).to_bytes().unwrap();
        assert_eq!(line, b"claude\r");
    }
}
