//! The MCP server entry that gives a Codex session the peer tools
//! (`marspot-peer mcp`), kept in every Codex home's `config.toml` the
//! way the Claude Code hooks are kept in its settings.
//!
//! One table, `[mcp_servers.marspot]`, ours and nobody else's.  The
//! file is edited as text: the table is appended when it is missing
//! and its `command` line is repointed when the helper has moved, and
//! every other byte stays as Codex and the user left it.  A file whose
//! entry is written some way this does not read is left alone and
//! logged.

use std::path::{Path, PathBuf};

const TABLE: &str = "mcp_servers.marspot";
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
fn config_files(home: &Path) -> Vec<PathBuf> {
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
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
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

fn reconcile_file(path: &Path, bin: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    match with_server(&text, bin) {
        Ok(None) => None,
        Ok(Some(new)) => match crate::plugins::claudecode::write_through_symlink(path, &new) {
            Ok(()) => Some("marspot MCP server set".into()),
            Err(e) => Some(e.to_string()),
        },
        Err(why) => Some(why.into()),
    }
}

/// `text` with the table present and its command `bin`; None when it
/// already is.  Err when the file defines the entry in a form other
/// than a `[mcp_servers.marspot]` table with a one-line string
/// `command`.
fn with_server(text: &str, bin: &str) -> Result<Option<String>, &'static str> {
    let mut table = String::new();
    let mut seen = false;
    let mut command: Option<std::ops::Range<usize>> = None;
    // the delimiter of a multi-line string the scan is inside of
    let mut open: Option<&str> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        if let Some(delim) = open {
            if line.matches(delim).count() % 2 == 1 {
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
        if table == TABLE && key == "command" {
            let lead = value.len() - value.trim_start().len();
            let len = string_token(value.trim_start()).ok_or(UNREADABLE)?;
            if command.is_some() {
                return Err(UNREADABLE);
            }
            command = Some(start + eq + 1 + lead..start + eq + 1 + lead + len);
        }
        open = ["\"\"\"", "'''"].into_iter().find(|d| value.matches(*d).count() % 2 == 1);
    }
    if open.is_some() {
        return Err(UNREADABLE);
    }
    if !seen {
        let mut out = text.to_string();
        if !out.is_empty() {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
        out.push_str(&format!("[{TABLE}]\ncommand = {}\nargs = [\"mcp\"]\n", toml_string(bin)));
        return Ok(Some(out));
    }
    let range = command.ok_or(UNREADABLE)?;
    if string_value(&text[range.clone()]).as_deref() == Some(bin) {
        return Ok(None);
    }
    Ok(Some(format!("{}{}{}", &text[..range.start], toml_string(bin), &text[range.end..])))
}

/// The table a header line names, without spaces and key quotes:
/// `[ mcp_servers."marspot" ] # x` is `mcp_servers.marspot`.
fn header(body: &str) -> String {
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
fn string_token(value: &str) -> Option<usize> {
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
fn string_value(token: &str) -> Option<String> {
    let inner = &token[1..token.len() - 1];
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

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "model = \"gpt-5\"\n\n[projects.\"/w/x\"]\ntrust_level = \"trusted\"\n";

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-codex-config-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn an_absent_table_is_appended_and_a_present_one_is_not_touched() {
        let d = dir();
        let f = d.join("config.toml");
        std::fs::write(&f, CONFIG).unwrap();
        assert_eq!(reconcile_file(&f, "/a/marspot-peer").as_deref(), Some("marspot MCP server set"));
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text, format!("{CONFIG}\n[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\n"));
        assert_eq!(reconcile_file(&f, "/a/marspot-peer"), None);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), text, "byte for byte");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_file_is_created_and_one_without_a_final_newline_gains_it() {
        let d = dir();
        let f = d.join("config.toml");
        reconcile_file(&f, "/a/marspot-peer");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\n");
        let new = with_server("model = \"gpt-5\"", "/a/marspot-peer").unwrap().unwrap();
        assert_eq!(new, "model = \"gpt-5\"\n\n[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\n");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn another_path_is_repointed_and_nothing_else_moves() {
        let old = "a = 1\n[ mcp_servers.\"marspot\" ] # peers\n  command = '/old/marspot-peer'   # where\nargs = [\"mcp\", \"-v\"]\n[mcp_servers.marspot.env]\ncommand = \"not this one\"\n[tui]\nx = 1";
        let new = with_server(old, "/new dir/marspot-peer").unwrap().unwrap();
        assert_eq!(new, old.replace("'/old/marspot-peer'", "\"/new dir/marspot-peer\""));
        assert_eq!(with_server(&new, "/new dir/marspot-peer"), Ok(None));
        // a literal string naming the same path is the same path
        assert_eq!(with_server(old, "/old/marspot-peer"), Ok(None));
        // quotes and backslashes in a path survive the round trip
        let odd = with_server("", "/a \"b\"\\c/marspot-peer").unwrap().unwrap();
        assert_eq!(with_server(&odd, "/a \"b\"\\c/marspot-peer"), Ok(None));
    }

    #[test]
    fn a_table_header_inside_a_string_is_not_a_table() {
        let text = "notes = \"\"\"\n[mcp_servers.marspot]\ncommand = \"x\"\n\"\"\"\n";
        let new = with_server(text, "/a/marspot-peer").unwrap().unwrap();
        assert!(new.starts_with(text) && new.ends_with("[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\n"));
    }

    #[test]
    fn an_entry_in_another_form_is_refused() {
        for text in [
            "[mcp_servers]\nmarspot = { command = \"/x\", args = [\"mcp\"] }\n",
            "mcp_servers.marspot.command = \"/x\"\n",
            "[mcp_servers.marspot]\nargs = [\"mcp\"]\n",
            "[mcp_servers.marspot]\ncommand = \"\"\"/x\"\"\"\n",
            "[mcp_servers.marspot]\ncommand = \"/x\" \"y\"\n",
            "[mcp_servers.marspot]\ncommand = \"/x\"\n[mcp_servers.marspot]\ncommand = \"/y\"\n",
        ] {
            assert_eq!(with_server(text, "/a/marspot-peer"), Err(UNREADABLE), "{text}");
        }
        let d = dir();
        let f = d.join("config.toml");
        let text = "mcp_servers.marspot.command = \"/x\"\n";
        std::fs::write(&f, text).unwrap();
        assert_eq!(reconcile_file(&f, "/a/marspot-peer").as_deref(), Some(UNREADABLE));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), text);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_config_reached_by_several_names_is_edited_once() {
        let home = dir();
        let shared = home.join(".codex-shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("config.toml"), CONFIG).unwrap();
        for n in [1, 2] {
            let p = home.join(format!(".codex-profile-{n}"));
            std::fs::create_dir_all(&p).unwrap();
            std::os::unix::fs::symlink(shared.join("config.toml"), p.join("config.toml")).unwrap();
        }
        // the default home is a link to a profile; a third profile has
        // no config yet; a name that is not a profile is not looked at
        std::os::unix::fs::symlink(home.join(".codex-profile-1"), home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".codex-profile-3")).unwrap();
        std::fs::create_dir_all(home.join(".codex-profile-old")).unwrap();
        let files = config_files(&home);
        assert_eq!(files, vec![shared.join("config.toml"), home.join(".codex-profile-3/config.toml")]);
        for f in &files {
            reconcile_file(f, "/a/marspot-peer");
        }
        let text = std::fs::read_to_string(shared.join("config.toml")).unwrap();
        assert_eq!(text.matches("[mcp_servers.marspot]").count(), 1);
        assert!(std::fs::symlink_metadata(home.join(".codex-profile-2/config.toml")).unwrap().file_type().is_symlink());
        assert!(home.join(".codex-profile-3/config.toml").is_file());
        assert!(config_files(&home.join("nobody")).is_empty(), "a home that is not there gets nothing");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
