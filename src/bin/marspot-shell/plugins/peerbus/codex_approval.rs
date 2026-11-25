//! The approval mode of marspot's MCP server in a Codex home, as a
//! helper's start reads it.

use std::path::Path;

use marspot_term::lx_info;

use super::codex_config::{APPROVAL, APPROVED, OPEN, TABLE, scan};
use super::codex_toml::{closes, header, opened_by, string_token, string_value, toml_string};

/// The codex arguments under which a session in `home` is asked about
/// no tool of marspot's MCP server, however its config has the server:
/// none where the config has a mode, the user's own too.  A session
/// given any has no inbox (see `APPROVAL`).
pub(crate) fn approved_args(home: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(home.join("config.toml")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Vec::new(),
    };
    if let Some(mode) = mode_said(&text)
        && string_value(&mode).as_deref() != Some(APPROVED)
    {
        lx_info!("peerbus.codex_config.mode_kept", "the account's own approval mode for marspot's tools is kept", home = home.display().to_string(), mode = mode);
    }
    let bin = super::inbox::peer_bin();
    approved(&text, bin.as_deref().and_then(Path::to_str))
}

/// codex starts with no override that is all it has of a server: where
/// the file has no server, the override gives a whole one.
pub(super) fn approved(text: &str, bin: Option<&str>) -> Vec<String> {
    if mode_said(text).is_some() {
        return Vec::new();
    }
    let mut sets = vec![format!("{TABLE}.{APPROVAL}=\"{APPROVED}\"")];
    match (scan(text), bin) {
        (Err(why), _) if why == OPEN => return Vec::new(),
        // there, in a form of its own
        (Err(_), _) => {}
        (Ok(Some(_)), _) => {}
        (Ok(None), None) => return Vec::new(),
        (Ok(None), Some(b)) => sets.extend([format!("{TABLE}.command={}", toml_string(b)), format!("{TABLE}.args=[\"mcp\"]")]),
    }
    sets.into_iter().flat_map(|s| ["-c".to_string(), s]).collect()
}

/// The approval mode `text` gives the server, as written, in whatever
/// form the entry is.
pub(super) fn mode_said(text: &str) -> Option<String> {
    let mut table = String::new();
    // the delimiter of a multi-line string the scan is inside of
    let mut open: Option<&str> = None;
    for line in text.lines().map(str::trim) {
        if let Some(delim) = open {
            if closes(line, delim) {
                open = None;
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            table = header(line);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key: String = key.chars().filter(|c| !matches!(c, ' ' | '\t' | '"' | '\'')).collect();
        let full = if table.is_empty() { key } else { format!("{table}.{key}") };
        let value = value.trim();
        if full.strip_prefix(TABLE).and_then(|k| k.strip_prefix('.')) == Some(APPROVAL) {
            let end = string_token(value).unwrap_or(value.len());
            return Some(value[..end].to_string());
        }
        // an inline table, of the server or of all servers
        let inline = full == TABLE || (full == "mcp_servers" && value.contains("marspot"));
        if inline && let Some(at) = value.find(APPROVAL) {
            let said = value[at + APPROVAL.len()..].trim_start_matches(['"', '\'', ' ', '\t', '=']);
            let end = said.find(['"', '\'', ',', '}']).unwrap_or(said.len());
            return Some(toml_string(said[..end].trim()));
        }
        open = opened_by(value);
    }
    None
}
