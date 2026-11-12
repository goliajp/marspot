//! What a Kimi session needs to be a peer, kept in every Kimi home
//! the way the other agents' entries are kept in theirs: the MCP
//! server that gives it the peer tools (`mcpServers.marspot` in
//! `mcp.json`, running `marspot-peer mcp`) and the hook that hands it
//! the messages waiting for it (two `[[hooks]]` tables in
//! `config.toml`, running `marspot-peer kimi-hook`).
//!
//! Those entries are ours and nobody else's.  `mcp.json` is rewritten
//! value for value, like Gemini's settings.  `config.toml` is edited as
//! text: our tables are appended when they are missing and their
//! `command` line is repointed when the helper has moved, and every
//! other byte stays as Kimi and the user left it.  Kimi refuses a
//! config with a key it does not know, so a table of ours holds
//! `event`, `command` and `timeout` and nothing else.

use std::path::{Path, PathBuf};

use marspot_peer::json::{self, Value, obj};
use marspot_peer::kimi_hook::EVENTS;

use super::claude_hooks::{pretty, slot};

const SERVER: &str = "marspot";
/// Seconds Kimi waits for the hook.  It reads a few small files.
const TIMEOUT: u32 = 10;
const UNREADABLE: &str = "`hooks` is not written as [[hooks]] tables; the marspot hooks are left out";

/// Add or repoint the entries in every Kimi home.  Returns lines worth
/// logging.
pub(super) fn reconcile() -> Vec<String> {
    let (Some(bin), Some(home)) = (super::inbox::peer_bin(), std::env::var_os("HOME")) else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    // a sandbox normally shares the developer's HOME, and those are
    // the developer's own Kimi homes; one given a HOME of its own may
    // write there
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() && account_home().is_none_or(|own| same_dir(&own, &home)) {
        return Vec::new();
    }
    let Some(bin) = bin.to_str() else { return Vec::new() };
    let mut notes = Vec::new();
    for dir in homes(&home) {
        notes.extend(reconcile_mcp(&dir.join("mcp.json"), bin).map(|n| format!("{}: {n}", dir.join("mcp.json").display())));
        notes.extend(reconcile_hooks(&dir.join("config.toml"), bin).map(|n| format!("{}: {n}", dir.join("config.toml").display())));
    }
    notes
}

/// The home directory the account database gives this user, whatever
/// `HOME` says.
fn account_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf: Vec<libc::c_char> = vec![0; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut found) };
    if r != 0 || found.is_null() || pw.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    real(a) == real(b)
}

/// `~/.kimi-code` and every `~/.kimi-code-profile-N` that exists, each
/// real directory once.  A home that does not exist is not created:
/// on a machine that never ran kimi there is nothing to do.
pub(super) fn homes(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![crate::plugins::kimi::default_home(home)];
    dirs.extend(crate::plugins::kimi::profile_dirs_in(home).into_iter().map(|(_, dir)| dir));
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(real) = dir.canonicalize() else { continue };
        if real.is_dir() && !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

fn reconcile_mcp(path: &Path, bin: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(_) => return None,
    };
    let Ok(mut v) = json::parse(&text) else {
        return Some("not plain JSON; the marspot server is left out".into());
    };
    let obj_or_absent = |x: Option<&Value>| x.is_none_or(|x| matches!(x, Value::Obj(_)));
    if !matches!(v, Value::Obj(_)) || !obj_or_absent(v.get("mcpServers")) || !obj_or_absent(v.at(&["mcpServers", SERVER])) {
        return Some("not the shape of Kimi's MCP file; the marspot server is left out".into());
    }
    if !with_server(&mut v, bin) {
        return None;
    }
    let mut new_text = pretty(&v);
    if text.ends_with('\n') {
        new_text.push('\n');
    }
    match crate::plugins::claudecode::write_through_symlink(path, &new_text) {
        Ok(()) => Some("peer tools set".into()),
        Err(e) => Some(e.to_string()),
    }
}

/// Our server in `v`, added or repointed at `bin`.  True when anything
/// changed.  A key the user added to the entry stays.
fn with_server(v: &mut Value, bin: &str) -> bool {
    let server = slot(slot(v, "mcpServers", || obj([])), SERVER, || obj([]));
    let args = Value::Arr(vec![Value::from("mcp")]);
    let mut changed = false;
    if server.str_at("command") != Some(bin) {
        server.set("command", Value::from(bin));
        changed = true;
    }
    if server.get("args") != Some(&args) {
        server.set("args", args);
        changed = true;
    }
    changed
}

fn reconcile_hooks(path: &Path, bin: &str) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    // kimi runs a hook's command through the shell
    let command = format!("{} kimi-hook", crate::plugins::claudecode::sh_quote(bin));
    match with_hooks(&text, &command) {
        Ok(None) => None,
        Ok(Some(new_text)) => match crate::plugins::claudecode::write_through_symlink(path, &new_text) {
            Ok(()) => Some("inbox hooks set".into()),
            Err(e) => Some(e.to_string()),
        },
        Err(why) => Some(why.into()),
    }
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The value of `key = …` on this line, as written.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    Some(line.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?.trim())
}

/// `text` with our hook tables added or repointed at `command`; None
/// when it already has them.
fn with_hooks(text: &str, command: &str) -> Result<Option<String>, &'static str> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let header = |l: &str| l.trim_start().starts_with('[');
    // an inline `hooks = [...]` cannot be extended with tables
    if lines.iter().take_while(|l| !header(l)).any(|l| value_of(l, "hooks").is_some()) {
        return Err(UNREADABLE);
    }
    // our tables: the event each is for and where its command line is
    let mut ours: Vec<(String, usize)> = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        if lines[at].trim() != "[[hooks]]" {
            at += 1;
            continue;
        }
        let end = (at + 1..lines.len()).find(|&i| header(&lines[i])).unwrap_or(lines.len());
        let find = |key: &str| (at + 1..end).find(|&i| value_of(&lines[i], key).is_some());
        if let (Some(e), Some(c)) = (find("event"), find("command"))
            && lines[c].contains("marspot-peer")
            && lines[c].contains("kimi-hook")
        {
            ours.push((value_of(&lines[e], "event").unwrap_or_default().trim_matches('"').to_string(), c));
        }
        at = end;
    }
    let want = format!("command = {}", toml_string(command));
    let mut changed = false;
    let mut missing = Vec::new();
    for event in EVENTS {
        match ours.iter().find(|(e, _)| e == event) {
            Some((_, c)) if lines[*c].trim() == want => {}
            Some((_, c)) => {
                lines[*c] = format!("{want}\n");
                changed = true;
            }
            None => missing.push(*event),
        }
    }
    let mut out: String = lines.concat();
    for event in missing {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[[hooks]]\nevent = \"{event}\"\n{want}\ntimeout = {TIMEOUT}\n"));
        changed = true;
    }
    Ok(changed.then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    const CMD: &str = "'/Users/x/.local/bin/marspot-peer' kimi-hook";

    #[test]
    fn the_hooks_are_appended_once_and_everything_else_stays() {
        let user = "default_model = \"kimi-code/k3\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"/usr/local/bin/notify done\"\ntimeout = 5\n\n[providers.x]\ntype = \"kimi\"";
        let new = with_hooks(user, CMD).unwrap().expect("changed");
        assert!(new.starts_with(user), "the user's text is a prefix of the result:\n{new}");
        let added = &new[user.len()..];
        assert_eq!(
            added,
            "\n\n[[hooks]]\nevent = \"UserPromptSubmit\"\ncommand = \"'/Users/x/.local/bin/marspot-peer' kimi-hook\"\ntimeout = 10\n\
             \n[[hooks]]\nevent = \"Stop\"\ncommand = \"'/Users/x/.local/bin/marspot-peer' kimi-hook\"\ntimeout = 10\n"
        );
        assert_eq!(with_hooks(&new, CMD), Ok(None), "a second pass changes nothing");
    }

    #[test]
    fn an_empty_config_gets_only_the_two_tables() {
        let new = with_hooks("", CMD).unwrap().unwrap();
        assert!(new.starts_with("[[hooks]]\nevent = \"UserPromptSubmit\"\n"), "{new}");
        assert_eq!(new.matches("[[hooks]]").count(), 2);
        // only the keys kimi accepts in a hook table
        for line in new.lines().filter(|l| !l.is_empty() && !l.starts_with('[')) {
            assert!(["event", "command", "timeout"].iter().any(|k| value_of(line, k).is_some()), "{line}");
        }
    }

    #[test]
    fn a_helper_that_moved_is_repointed_in_place() {
        let old = with_hooks("a = 1\n", "'/old/marspot-peer' kimi-hook").unwrap().unwrap();
        let new = with_hooks(&old, CMD).unwrap().expect("repointed");
        assert!(!new.contains("/old/"), "{new}");
        assert_eq!(new.matches("[[hooks]]").count(), 2, "no table was added");
        assert_eq!(new.len() - CMD.len() * 2, old.len() - "'/old/marspot-peer' kimi-hook".len() * 2, "nothing else moved");
    }

    #[test]
    fn hooks_written_inline_are_left_alone() {
        assert_eq!(with_hooks("hooks = []\n", CMD), Err(UNREADABLE));
        // a key of that name inside a table is something else
        assert!(with_hooks("[tool]\nhooks = 1\n", CMD).unwrap().is_some());
    }

    #[test]
    fn a_path_is_written_as_a_toml_string() {
        assert_eq!(toml_string(r#"'/a "b"\c' kimi-hook"#), r#""'/a \"b\"\\c' kimi-hook""#);
    }

    #[test]
    fn the_server_is_added_beside_the_users_own_and_only_once() {
        let mut v = json::parse(r#"{"mcpServers":{"mine":{"command":"x"}},"other":1}"#).unwrap();
        assert!(with_server(&mut v, "/Users/x/.local/bin/marspot-peer"));
        assert_eq!(v.at(&["mcpServers", "mine", "command"]).and_then(Value::str), Some("x"));
        assert_eq!(v.at(&["mcpServers", SERVER, "command"]).and_then(Value::str), Some("/Users/x/.local/bin/marspot-peer"));
        assert!(!with_server(&mut v, "/Users/x/.local/bin/marspot-peer"), "a second pass changes nothing");
        assert!(with_server(&mut v, "/elsewhere/marspot-peer"), "a helper that moved is repointed");
    }

    #[test]
    fn both_files_are_written_into_a_home_and_a_home_kimi_never_made_is_not() {
        let user = tmp("kimi-config-homes");
        assert_eq!(homes(&user), Vec::<PathBuf>::new(), "kimi never ran here");
        let home = user.join(".kimi-code");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&home, user.join(".kimi-code-profile-1")).unwrap();
        assert_eq!(homes(&user), vec![home.canonicalize().unwrap()], "a profile that links to the default is one home");
        assert_eq!(reconcile_mcp(&home.join("mcp.json"), "/b/marspot-peer").as_deref(), Some("peer tools set"));
        assert_eq!(reconcile_hooks(&home.join("config.toml"), "/b/marspot-peer").as_deref(), Some("inbox hooks set"));
        assert_eq!(reconcile_mcp(&home.join("mcp.json"), "/b/marspot-peer"), None);
        assert_eq!(reconcile_hooks(&home.join("config.toml"), "/b/marspot-peer"), None);
        std::fs::write(home.join("mcp.json"), "// comment\n{}").unwrap();
        assert!(reconcile_mcp(&home.join("mcp.json"), "/b/marspot-peer").unwrap().contains("not plain JSON"));
    }
}
