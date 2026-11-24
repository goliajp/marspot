//! The MCP server entry that gives a Codex session the peer tools
//! (`marspot-peer mcp`), kept in every Codex home's `config.toml` the
//! way the Claude Code hooks are kept in its settings.
//!
//! One table, `[mcp_servers.marspot]`, ours and nobody else's.  The
//! file is edited as text: the table is appended when it is missing,
//! its `command` line is repointed when the helper has moved, its
//! tools' approval is set where the file says nothing of it, and every
//! other byte stays as Codex and the user left it.  A file whose
//! entry is written some way this does not read is left alone and
//! logged.

use std::ops::Range;
use std::path::{Path, PathBuf};

pub(super) const TABLE: &str = "mcp_servers.marspot";
/// Whether codex asks before a tool of the server runs.  In the file
/// and not on a helper's command line: codex (0.171) started with a
/// `-c` like it leaves its home's app-server for one inside the TUI,
/// whose threads nothing can be delivered into.
pub(super) const APPROVAL: &str = "default_tools_approval_mode";
pub(super) const APPROVED: &str = "approve";
/// A file whose end is inside a string, as far as this reads it.
pub(super) const OPEN: &str = "a string this cannot follow; left as it is";
const UNREADABLE: &str = "a marspot MCP entry this cannot read; left as it is";

/// Add or repoint the entry in every Codex home.  Returns lines worth
/// logging.
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
    config_files(Path::new(&home))
        .iter()
        .filter_map(|f| reconcile_file(f, bin).map(|note| format!("{}: {note}", f.display())))
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

fn reconcile_file(path: &Path, bin: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    match with_server(&text, bin) {
        Ok(None) => None,
        Ok(Some(new)) => match crate::plugins::claudecode::write_through_symlink(path, &new) {
            Ok(()) => Some(edit_note(&text).into()),
            Err(e) => Some(e.to_string()),
        },
        Err(why) => Some(why.into()),
    }
}

/// What `with_server` did to `text`, which it changed.
fn edit_note(text: &str) -> &'static str {
    match scan(text) {
        Ok(Some(t)) if t.approval.is_some() => "marspot MCP server repointed",
        Ok(Some(_)) => "marspot MCP server's tools approved: default_tools_approval_mode = \"approve\" added",
        _ => "marspot MCP server set, its tools approved: default_tools_approval_mode = \"approve\"",
    }
}

/// The `[mcp_servers.marspot]` table of a config.
pub(super) struct Table {
    /// The `command` value, where it is in the text.
    command: Range<usize>,
    /// Where the line of `command` ends, its line break included.
    command_line_end: usize,
    /// The approval as written; empty when it is no one-line string.
    approval: Option<String>,
}

/// The table in `text`, if there is one.  Err when the file defines the
/// entry in a form other than a `[mcp_servers.marspot]` table with a
/// one-line string `command`.
pub(super) fn scan(text: &str) -> Result<Option<Table>, &'static str> {
    let mut table = String::new();
    let mut seen = false;
    let mut command: Option<(Range<usize>, usize)> = None;
    let mut approval: Option<String> = None;
    // the delimiter of a multi-line string the scan is inside of
    let mut open: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        if let Some(delim) = open {
            if closes(line, delim) {
                open = None;
            }
            continue;
        }
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if body.starts_with('[') {
            table = header(body);
            if table == TABLE {
                if seen {
                    return Err(UNREADABLE);
                }
                seen = true;
            }
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key: String = line[..eq].chars().filter(|c| !matches!(c, ' ' | '\t' | '"' | '\'')).collect();
        let value = &line[eq + 1..];
        // the same entry written as a dotted key or an inline table
        let dotted = match table.as_str() {
            "" => key == TABLE || key.starts_with("mcp_servers.marspot."),
            "mcp_servers" => key == "marspot" || key.starts_with("marspot."),
            _ => false,
        };
        if dotted {
            return Err(UNREADABLE);
        }
        let token = || {
            let lead = value.len() - value.trim_start().len();
            let from = start + eq + 1 + lead;
            string_token(value.trim_start()).map(|len| from..from + len)
        };
        if table == TABLE && key == "command" {
            let range = token().ok_or(UNREADABLE)?;
            if command.is_some() {
                return Err(UNREADABLE);
            }
            command = Some((range, at));
        }
        if table == TABLE && key == APPROVAL {
            approval = Some(token().map_or(String::new(), |r| text[r].to_string()));
        }
        open = opened_by(value);
    }
    if open.is_some() {
        return Err(OPEN);
    }
    if !seen {
        return Ok(None);
    }
    let (command, command_line_end) = command.ok_or(UNREADABLE)?;
    Ok(Some(Table { command, command_line_end, approval }))
}

/// The delimiter of the multi-line string `value` opens and does not
/// close.
pub(super) fn opened_by(value: &str) -> Option<&'static str> {
    ["\"\"\"", "'''"].into_iter().find(|d| closes(value, d))
}

/// Whether `line` ends on the other side of a `delim` string than it
/// starts on.
pub(super) fn closes(line: &str, delim: &str) -> bool {
    line.matches(delim).count() % 2 == 1
}

/// `text` with the table present, its command `bin` and its tools'
/// approval said; None when it already is.  Err as `scan`.
fn with_server(text: &str, bin: &str) -> Result<Option<String>, &'static str> {
    let Some(t) = scan(text)? else {
        let mut out = text.to_string();
        if !out.is_empty() {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
        out.push_str(&format!("[{TABLE}]\ncommand = {}\nargs = [\"mcp\"]\n{APPROVAL} = \"{APPROVED}\"\n", toml_string(bin)));
        return Ok(Some(out));
    };
    let same = string_value(&text[t.command.clone()]).as_deref() == Some(bin);
    // a mode the file has is the user's choice
    if same && t.approval.is_some() {
        return Ok(None);
    }
    let command = if same { text[t.command.clone()].to_string() } else { toml_string(bin) };
    let mut out = format!("{}{command}{}", &text[..t.command.start], &text[t.command.end..t.command_line_end]);
    if t.approval.is_none() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{APPROVAL} = \"{APPROVED}\"\n"));
    }
    out.push_str(&text[t.command_line_end..]);
    Ok(Some(out))
}

/// The table a header line names, without spaces and key quotes:
/// `[ mcp_servers."marspot" ] # x` is `mcp_servers.marspot`.
pub(super) fn header(body: &str) -> String {
    let mut name = String::new();
    let mut quote: Option<char> = None;
    for c in body.trim_start_matches('[').chars() {
        match (quote, c) {
            (None, ']') => break,
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ' ' | '\t') => {}
            _ => name.push(c),
        }
    }
    name
}

/// The length of the one-line string `value` starts with, when the
/// string is all the line holds besides a comment.
pub(super) fn string_token(value: &str) -> Option<usize> {
    let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return None;
    }
    let mut escaped = false;
    let end = value[1..].char_indices().find_map(|(i, c)| {
        let closes = c == quote && !escaped;
        escaped = quote == '"' && c == '\\' && !escaped;
        closes.then_some(i + 2)
    })?;
    let rest = value[end..].trim();
    (rest.is_empty() || rest.starts_with('#')).then_some(end)
}

/// What a one-line string token says.  None for an escape other than
/// the two `toml_string` writes: such a value is not one written here,
/// and it is repointed.
pub(super) fn string_value(token: &str) -> Option<String> {
    let inner = token.get(1..token.len().checked_sub(1)?)?;
    if token.starts_with('\'') {
        return Some(inner.to_string());
    }
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().filter(|e| matches!(e, '\\' | '"'))?),
            c => out.push(c),
        }
    }
    Some(out)
}

pub(super) fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
use super::codex_approval::{approved, approved_args, mode_said};
#[cfg(test)]
#[path = "codex_config_tests.rs"]
mod tests;
