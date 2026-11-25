//! The MCP servers marspot registers with every agent, as one table
//! each agent's config writer renders in its own format: the peer
//! tools (`marspot-peer mcp`) always, and the madcawl browser tools
//! (`madcawl-mcp`) while that program is installed.
//!
//! `madcawl-mcp` is looked for at one fixed path, `~/.local/bin`, the
//! directory marspot already installs `marspot-peer` into: a path that
//! does not depend on the PATH of whichever process happens to run the
//! reconcile, and one the user's own account can write without a
//! package manager.  It reaches its host through `MADCAWL_SOCKET`, a
//! socket in marspot's state root.  An entry of a server that is no
//! longer installed is removed, but only one whose command is the
//! fixed path: an entry the user pointed elsewhere is theirs, and it is
//! not written over while the program is installed either.

use std::path::PathBuf;

use marspot_peer::json::{Value, obj};

use super::claude_hooks::slot;

pub(super) const PEER: &str = "marspot";
pub(super) const MADCAWL: &str = "madcawl";
const MADCAWL_BIN: &str = ".local/bin/madcawl-mcp";
const MADCAWL_SOCKET: &str = "madcawl.sock";
/// Why an entry under a server's name was left as the user wrote it.
pub(super) const NOT_OURS: &str = "an entry pointed at another program; left as it is";

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Server {
    pub name: &'static str,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    /// The command is a fixed path, so an entry under the name with
    /// another command is the user's own and stays as it is.  The peer
    /// helper's entry is repointed wherever the helper moves.
    pub fixed: bool,
}

impl Server {
    /// Whether an existing entry with `command` is to be written:
    /// always for a moving command, only when it is the fixed path
    /// otherwise.
    pub(super) fn owns(&self, command: Option<&str>) -> bool {
        !self.fixed || command == Some(self.command.as_str())
    }
}

/// The servers to register, and those to take out again: a name and
/// the command an entry of ours has.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Servers {
    pub list: Vec<Server>,
    pub gone: Vec<(&'static str, String)>,
}

/// The table with the peer helper at `peer_bin`, `madcawl-mcp` looked
/// for under `HOME` and given a socket in the state root.
pub(super) fn servers(peer_bin: &str) -> Servers {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let madcawl = home.map(|h| h.join(MADCAWL_BIN));
    let madcawl = madcawl.as_deref().and_then(|p| p.to_str().map(|s| (s, p.is_file())));
    let socket = marspot_term::paths::state_root().join(MADCAWL_SOCKET);
    match (madcawl, socket.to_str()) {
        (Some((bin, present)), Some(socket)) => of(peer_bin, Some((bin, present, socket))),
        _ => of(peer_bin, None),
    }
}

/// The table from what was found: `madcawl` is the fixed path, whether
/// a program is there, and the socket it is told.
pub(super) fn of(peer_bin: &str, madcawl: Option<(&str, bool, &str)>) -> Servers {
    let peer = Server { name: PEER, command: peer_bin.into(), args: vec!["mcp".into()], env: Vec::new(), fixed: false };
    let mut out = Servers { list: vec![peer], gone: Vec::new() };
    match madcawl {
        Some((bin, true, socket)) => out.list.push(Server {
            name: MADCAWL,
            command: bin.into(),
            args: Vec::new(),
            env: vec![("MADCAWL_SOCKET", socket.into())],
            fixed: true,
        }),
        Some((bin, false, _)) => out.gone.push((MADCAWL, bin.into())),
        None => {}
    }
    out
}

/// The path `madcawl-mcp` is looked for at, for the watch that notes
/// it being installed or removed.
pub(super) fn madcawl_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(MADCAWL_BIN))
}

/// Whether `servers` (an `mcpServers` value) is an object or absent,
/// and every entry `json_set` writes is an object or absent, with an
/// `env` object or none where the server carries an environment.  An
/// entry left alone, or one only taken out, is not looked at: taking
/// one out reads its command and nothing else.
pub(super) fn json_fits(servers: Option<&Value>, table: &Servers) -> bool {
    let is_obj = |x: Option<&Value>| x.is_none_or(|x| matches!(x, Value::Obj(_)));
    is_obj(servers)
        && table.list.iter().all(|s| {
            let entry = servers.and_then(|v| v.get(s.name));
            if entry.is_some_and(|e| !s.owns(e.str_at("command"))) {
                return true;
            }
            is_obj(entry) && (s.env.is_empty() || is_obj(entry.and_then(|e| e.get("env"))))
        })
}

/// A note naming the servers whose entry in `servers` is the user's
/// own and is left as it is; None when there is none.
pub(super) fn json_kept(servers: Option<&Value>, table: &Servers) -> Option<String> {
    let kept: Vec<String> = table
        .list
        .iter()
        .filter(|s| servers.and_then(|v| v.get(s.name)).is_some_and(|e| !s.owns(e.str_at("command"))))
        .map(|s| format!("{}: {NOT_OURS}", s.name))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// `note` followed by the `json_kept` note, if any.
pub(super) fn and_kept(note: String, kept: Option<String>) -> String {
    match kept {
        Some(k) => format!("{note}; {k}"),
        None => note,
    }
}

/// `servers` (an `mcpServers` object) with every entry of the table
/// added or repointed and every gone one of ours taken out.  A key the
/// user added to an entry stays.  True when anything changed.
pub(super) fn json_reconcile(servers: &mut Value, table: &Servers) -> bool {
    let mut changed = false;
    for s in &table.list {
        changed |= json_set(servers, s);
    }
    for (name, command) in &table.gone {
        let ours = servers.at(&[name, "command"]).and_then(Value::str) == Some(command);
        if ours && let Value::Obj(kv) = servers {
            kv.retain(|(k, _)| k != name);
            changed = true;
        }
    }
    changed
}

/// One entry: `command`, `args`, and `env` when the server has any.
/// An entry of the user's own under the name is left as it is.
pub(super) fn json_set(servers: &mut Value, s: &Server) -> bool {
    if servers.get(s.name).is_some_and(|e| !s.owns(e.str_at("command"))) {
        return false;
    }
    let entry = slot(servers, s.name, || obj([]));
    let mut changed = false;
    if entry.str_at("command") != Some(s.command.as_str()) {
        entry.set("command", Value::from(s.command.as_str()));
        changed = true;
    }
    let args = Value::Arr(s.args.iter().map(|a| Value::from(a.as_str())).collect());
    if entry.get("args") != Some(&args) {
        entry.set("args", args);
        changed = true;
    }
    if s.env.is_empty() {
        return changed;
    }
    let env = slot(entry, "env", || obj([]));
    for (k, v) in &s.env {
        if env.str_at(k) != Some(v.as_str()) {
            env.set(k, Value::from(v.as_str()));
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
pub(super) mod fixtures {
    use super::*;

    pub(in super::super) const MADCAWL_AT: &str = "/u/.local/bin/madcawl-mcp";
    pub(in super::super) const SOCKET: &str = "/s/madcawl.sock";

    /// The table with `madcawl-mcp` installed.
    pub(in super::super) fn with_madcawl(peer: &str) -> Servers {
        of(peer, Some((MADCAWL_AT, true, SOCKET)))
    }

    /// The table with `madcawl-mcp` not installed.
    pub(in super::super) fn without_madcawl(peer: &str) -> Servers {
        of(peer, Some((MADCAWL_AT, false, SOCKET)))
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use marspot_term::test_env::{EnvGuard, TempDir};

    #[test]
    fn mcp_table_has_madcawl_only_while_its_program_is_at_the_fixed_path() {
        let home = TempDir::new("mcp-table-home");
        let state = TempDir::new("mcp-table-state");
        let mut env = EnvGuard::new();
        env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", state.path());
        let bin = home.join(MADCAWL_BIN);
        let t = servers("/p/marspot-peer");
        assert_eq!(t.list.len(), 1);
        assert_eq!(t.gone, vec![(MADCAWL, bin.to_str().unwrap().to_string())]);

        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let t = servers("/p/marspot-peer");
        assert!(t.gone.is_empty());
        let m = &t.list[1];
        assert_eq!((m.name, m.command.as_str()), (MADCAWL, bin.to_str().unwrap()));
        assert!(m.args.is_empty(), "madcawl-mcp reads its socket from the environment when given no arguments");
        let socket = state.join("madcawl.sock");
        assert_eq!(m.env, vec![("MADCAWL_SOCKET", socket.to_str().unwrap().to_string())]);
    }

    #[test]
    fn mcp_json_entries_are_set_kept_and_removed_only_when_ours() {
        let mut v = marspot_peer::json::parse(r#"{"mine":{"command":"x"},"madcawl":{"command":"/u/.local/bin/madcawl-mcp","trust":true}}"#).unwrap();
        assert!(json_reconcile(&mut v, &with_madcawl("/p")));
        assert_eq!(v.at(&["madcawl", "env", "MADCAWL_SOCKET"]).and_then(Value::str), Some(SOCKET));
        assert_eq!(v.at(&["madcawl", "trust"]).and_then(Value::bool), Some(true), "a key the user added stays");
        assert_eq!(v.at(&["marspot", "command"]).and_then(Value::str), Some("/p"));
        assert!(!json_reconcile(&mut v, &with_madcawl("/p")), "a second pass changes nothing");
        assert!(json_reconcile(&mut v, &without_madcawl("/p")));
        assert!(v.get("madcawl").is_none());
        assert!(v.get("mine").is_some());
        // one the user pointed elsewhere is not ours to remove
        let mut v = marspot_peer::json::parse(r#"{"madcawl":{"command":"/dev/madcawl-mcp"}}"#).unwrap();
        assert!(json_reconcile(&mut v, &without_madcawl("/p")));
        assert_eq!(v.at(&["madcawl", "command"]).and_then(Value::str), Some("/dev/madcawl-mcp"));
    }

    #[test]
    fn mcp_json_that_does_not_fit_is_refused() {
        let t = with_madcawl("/p");
        let ours_env_list = format!(r#"{{"madcawl":{{"command":"{MADCAWL_AT}","env":[]}}}}"#);
        for odd in [r#"[]"#, ours_env_list.as_str(), r#"{"marspot":1}"#] {
            assert!(!json_fits(Some(&marspot_peer::json::parse(odd).unwrap()), &t), "{odd}");
        }
        assert!(json_fits(None, &t));
    }

    /// Shapes only a write would trip over do not refuse a file where
    /// nothing is written: an entry the user owns, a gone one, and an
    /// `env` of a server that carries none.
    #[test]
    fn mcp_json_that_is_not_written_does_not_refuse_the_file() {
        let fits = |s: &str, t: &Servers| json_fits(Some(&marspot_peer::json::parse(s).unwrap()), t);
        for s in [r#"{"madcawl":"off"}"#, r#"{"madcawl":{"command":"/dev/m","env":[]}}"#, r#"{"marspot":{"env":"x"}}"#] {
            assert!(fits(s, &with_madcawl("/p")), "{s}");
            assert!(fits(s, &without_madcawl("/p")), "{s}");
        }
    }

    /// An entry under the name whose command is not the fixed path is
    /// the user's: not repointed, its args and env not reset, and named
    /// in a note.  Ours, or none, is written.
    #[test]
    fn mcp_json_entry_of_the_users_own_is_left_alone() {
        let user = r#"{"madcawl":{"command":"/dev/madcawl-mcp","args":["--x"],"env":{"MADCAWL_SOCKET":"/mine"}}}"#;
        let mut v = marspot_peer::json::parse(user).unwrap();
        let before = v.get("madcawl").cloned();
        assert!(json_reconcile(&mut v, &with_madcawl("/p")), "the peer entry is added");
        assert_eq!(v.get("madcawl").cloned(), before);
        assert_eq!(json_kept(Some(&v), &with_madcawl("/p")), Some(format!("madcawl: {NOT_OURS}")));
        assert!(!json_reconcile(&mut v, &with_madcawl("/p")));
        // the peer entry is repointed wherever it pointed
        let mut v = marspot_peer::json::parse(r#"{"marspot":{"command":"/old"}}"#).unwrap();
        assert!(json_reconcile(&mut v, &with_madcawl("/p")));
        assert_eq!(v.at(&["marspot", "command"]).and_then(Value::str), Some("/p"));
        assert_eq!(json_kept(Some(&v), &with_madcawl("/p")), None);
    }
}
