//! One server's table in a Codex `config.toml`, written, brought up to
//! date and taken out again as text.

use super::codex_config::{APPROVAL, APPROVED, UNREADABLE, scan_named};
use super::codex_inline::with_pairs;
use super::codex_toml::{string_value, toml_string};
use super::mcp_servers::{NOT_OURS, Server};

/// `{ K = "v" }`, the environment as this writes it.
fn env_value(s: &Server) -> String {
    let pairs: Vec<String> = s.env.iter().map(|(k, v)| format!("{k} = {}", toml_string(v))).collect();
    format!("{{ {} }}", pairs.join(", "))
}

/// The whole table of `s`, as appended.
fn table_text(s: &Server) -> String {
    let args: Vec<String> = s.args.iter().map(|a| toml_string(a)).collect();
    let env = if s.env.is_empty() { String::new() } else { format!("env = {}\n", env_value(s)) };
    format!(
        "[mcp_servers.{}]\ncommand = {}\nargs = [{}]\n{env}{APPROVAL} = \"{APPROVED}\"\n",
        s.name,
        toml_string(&s.command),
        args.join(", ")
    )
}

/// `text` with the table of `s` present, its command and environment
/// those of `s` and its tools' approval said; None when it already is.
/// Err as `scan_named`, and when the table is the user's own (`owns`).
pub(super) fn with_entry(text: &str, s: &Server) -> Result<Option<String>, &'static str> {
    let Some(t) = scan_named(text, s.name)? else {
        let mut out = text.to_string();
        if !out.is_empty() {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
        out.push_str(&table_text(s));
        return Ok(Some(out));
    };
    let same = string_value(&text[t.command.clone()]).as_deref() == Some(s.command.as_str());
    if !s.owns(same.then_some(s.command.as_str())) {
        return Err(NOT_OURS);
    }
    let mut env_line = None;
    if !s.env.is_empty() {
        match &t.env {
            Some(r) if !text[r.clone()].starts_with('{') => return Err(UNREADABLE),
            Some(r) => {
                if let Some(new) = with_pairs(&text[r.clone()], &s.env) {
                    let fixed = format!("{}{new}{}", &text[..r.start], &text[r.end..]);
                    return Ok(Some(with_entry(&fixed, s)?.unwrap_or(fixed)));
                }
            }
            // `[mcp_servers.<name>.env]` or `env.K = …`: a second `env`
            // would be refused
            None if t.env_table => return Err(UNREADABLE),
            None => env_line = Some(format!("env = {}\n", env_value(s))),
        }
    }
    // a mode the file has is the user's choice
    if same && t.approval.is_some() && env_line.is_none() {
        return Ok(None);
    }
    let command = if same { text[t.command.clone()].to_string() } else { toml_string(&s.command) };
    let mut out = format!("{}{command}{}", &text[..t.command.start], &text[t.command.end..t.command_line_end]);
    let approval = t.approval.is_none().then(|| format!("{APPROVAL} = \"{APPROVED}\"\n"));
    if env_line.is_some() || approval.is_some() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.extend(env_line);
        out.extend(approval);
    }
    out.push_str(&text[t.command_line_end..]);
    Ok(Some(out))
}

/// `text` without the table of server `name` when its command is
/// `command`, the one this writes; None when there is no such table.
pub(super) fn without_entry(text: &str, name: &str, command: &str) -> Result<Option<String>, &'static str> {
    let Some(t) = scan_named(text, name)? else { return Ok(None) };
    if string_value(&text[t.command.clone()]).as_deref() != Some(command) {
        return Ok(None);
    }
    let mut out = format!("{}{}", &text[..t.block.start], &text[t.block.end..]);
    // the blank line an append put before the table goes with it
    if t.block.end == text.len() && out.ends_with("\n\n") {
        out.pop();
    }
    Ok(Some(out))
}

/// The peer server alone, as written before the table.
#[cfg(test)]
pub(super) fn with_server(text: &str, bin: &str) -> Result<Option<String>, &'static str> {
    with_entry(text, &super::mcp_servers::of(bin, None).list[0])
}
