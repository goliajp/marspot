//! The MCP server entries of marspot's table (`mcp_servers`): the one
//! that gives a Codex session the peer tools (`marspot-peer mcp`), and
//! the madcawl browser tools while those are installed, kept in every
//! Codex home's `config.toml` the way the Claude Code hooks are kept in
//! its settings.
//!
//! One table per server, `[mcp_servers.<name>]`, ours and nobody
//! else's.  The file is edited as text: a table is appended when it is
//! missing, its `command` line is repointed when the program has moved
//! (only the peer helper's: an entry of a fixed-path server pointed
//! elsewhere is the user's and left whole), the pairs of its inline
//! `env` table that say something else are rewritten, its tools'
//! approval is set where the file says nothing of it, and every other
//! byte stays as Codex and the user left it.  A server no longer
//! installed has its table taken out.  A file whose entry is written
//! some way this does not read is left alone and logged.

use std::path::{Path, PathBuf};

use super::codex_entry::{with_entry, without_entry};
pub(super) use super::codex_scan::{Table, scan_named};
use super::mcp_servers::{PEER, Servers, servers};

pub(super) const TABLE: &str = "mcp_servers.marspot";
/// Whether codex asks before a tool of the server runs.  In the file
/// and not on a helper's command line: codex (0.171) started with a
/// `-c` like it leaves its home's app-server for one inside the TUI,
/// whose threads nothing can be delivered into.
pub(super) const APPROVAL: &str = "default_tools_approval_mode";
pub(super) const APPROVED: &str = "approve";
/// A file whose end is inside a string, as far as this reads it.
pub(super) const OPEN: &str = "a string this cannot follow; left as it is";
pub(super) const UNREADABLE: &str = "a marspot MCP entry this cannot read; left as it is";

/// Add, repoint or take out the entries in every Codex home.  Returns
/// lines worth logging.
pub(super) fn reconcile() -> Vec<String> {
    // a sandbox shares the developer's HOME, and these are the
    // developer's own Codex homes
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return Vec::new();
    }
    let (Some(bin), Some(home)) = (super::inbox::peer_bin(), std::env::var_os("HOME")) else {
        return Vec::new();
    };
    let Some(bin) = bin.to_str() else { return Vec::new() };
    let table = servers(bin);
    config_files(Path::new(&home))
        .iter()
        .filter_map(|f| reconcile_file(f, &table).map(|note| format!("{}: {note}", f.display())))
        .collect()
}

/// `config.toml` of `~/.codex` and of every `~/.codex-profile-N` that
/// exists, each real file once: profiles that link to one shared
/// config, or a default home that is a link to a profile, would
/// otherwise have it edited once per name.
pub(super) fn config_files(home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in home_dirs(home) {
        // a home that does not exist is not created
        let Ok(real_dir) = dir.canonicalize() else { continue };
        if !real_dir.is_dir() {
            continue;
        }
        let file = dir.join("config.toml");
        let real = match file.canonicalize() {
            Ok(real) => real,
            Err(_) if std::fs::symlink_metadata(&file).is_err() => real_dir.join("config.toml"),
            // a link to nowhere: not ours to replace
            Err(_) => continue,
        };
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

/// `~/.codex` and every `~/.codex-profile-N`, by name, whether or not
/// they exist.
pub(super) fn home_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".codex")];
    if let Ok(rd) = std::fs::read_dir(home) {
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| {
                e.file_name().to_str().and_then(|n| n.strip_prefix(".codex-profile-")).is_some_and(|n| n.parse::<u8>().is_ok())
            })
            .map(|e| e.path())
            .collect();
        profiles.sort();
        dirs.extend(profiles);
    }
    dirs
}

pub(super) fn reconcile_file(path: &Path, table: &Servers) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    let (new, notes) = with_table(&text, table);
    if new != text
        && let Err(e) = crate::plugins::claudecode::write_through_symlink(path, &new)
    {
        return Some(e.to_string());
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// `text` with every server of the table in it and every gone one of
/// ours out, and what was done or left undone.
pub(super) fn with_table(text: &str, table: &Servers) -> (String, Vec<String>) {
    let mut cur = text.to_string();
    let mut notes = Vec::new();
    let named = |name: &str, why: &str| if name == PEER { why.to_string() } else { format!("{name}: {why}") };
    for s in &table.list {
        match with_entry(&cur, s) {
            Ok(None) => {}
            Ok(Some(new)) => {
                notes.push(edit_note(&cur, s.name));
                cur = new;
            }
            Err(why) => notes.push(named(s.name, why)),
        }
    }
    for (name, command) in &table.gone {
        match without_entry(&cur, name, command) {
            Ok(None) => {}
            Ok(Some(new)) => {
                notes.push(format!("{name} MCP server taken out: {command} is gone"));
                cur = new;
            }
            Err(why) => notes.push(named(name, why)),
        }
    }
    (cur, notes)
}

/// What `with_entry` did to `text`, which it changed.
fn edit_note(text: &str, name: &str) -> String {
    match scan_named(text, name) {
        Ok(Some(t)) if t.approval.is_some() => format!("{name} MCP server repointed"),
        Ok(Some(_)) => format!("{name} MCP server's tools approved: {APPROVAL} = \"{APPROVED}\" added"),
        _ => format!("{name} MCP server set, its tools approved: {APPROVAL} = \"{APPROVED}\""),
    }
}

/// The marspot table in `text`, if there is one.
pub(super) fn scan(text: &str) -> Result<Option<Table>, &'static str> {
    scan_named(text, PEER)
}

#[cfg(test)]
use super::codex_approval::{approved, approved_args, mode_said};
#[cfg(test)]
use super::codex_entry::with_server;
#[cfg(test)]
#[path = "codex_config_tests.rs"]
mod tests;
