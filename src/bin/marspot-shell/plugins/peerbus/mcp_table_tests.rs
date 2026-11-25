//! The table of MCP servers as each agent's config writer renders it:
//! with `madcawl-mcp` absent every file is what it was before the
//! table, byte for byte; with it present each agent gets an entry
//! carrying the socket; taken away again, the entry goes.

use std::path::PathBuf;

use marspot_peer::json::{self, Value};
use marspot_term::test_env::{EnvGuard, TempDir};

use super::mcp_servers::fixtures::{MADCAWL_AT, SOCKET, with_madcawl, without_madcawl};
use super::mcp_servers::{MADCAWL, of};
use super::{claude_mcp, codex_config, gemini_config, kimi_config};

const PEER_BIN: &str = "/a/marspot-peer";
const CODEX: &str = "model = \"gpt-5\"\n\n[projects.\"/w/x\"]\ntrust_level = \"trusted\"\n";

fn codex(text: &str, table: &super::mcp_servers::Servers) -> String {
    codex_config::with_table(text, table).0
}

#[test]
fn mcp_table_codex_without_madcawl_is_byte_identical_to_the_peer_alone() {
    for text in ["", CODEX, "a = 1"] {
        assert_eq!(codex(text, &without_madcawl(PEER_BIN)), codex(text, &of(PEER_BIN, None)), "{text}");
    }
    let (_, notes) = codex_config::with_table(&codex(CODEX, &of(PEER_BIN, None)), &without_madcawl(PEER_BIN));
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn mcp_table_codex_gets_a_madcawl_table_and_loses_it_again() {
    let before = codex(CODEX, &of(PEER_BIN, None));
    let with = codex(&before, &with_madcawl(PEER_BIN));
    assert_eq!(
        with,
        format!(
            "{before}\n[mcp_servers.madcawl]\ncommand = \"{MADCAWL_AT}\"\nargs = []\nenv = {{ MADCAWL_SOCKET = \"{SOCKET}\" }}\ndefault_tools_approval_mode = \"approve\"\n"
        )
    );
    let (again, notes) = codex_config::with_table(&with, &with_madcawl(PEER_BIN));
    assert_eq!((again.as_str(), notes.len()), (with.as_str(), 0), "a second pass changes nothing");
    let (gone, notes) = codex_config::with_table(&with, &without_madcawl(PEER_BIN));
    assert_eq!(gone, before, "taken out byte for byte");
    assert_eq!(notes, [format!("madcawl MCP server taken out: {MADCAWL_AT} is gone")]);
}

#[test]
fn mcp_table_codex_madcawl_env_is_repointed_and_a_table_elsewhere_kept() {
    let user = "[mcp_servers.madcawl]\ncommand = \"/u/.local/bin/madcawl-mcp\" # mine\nenv = { MADCAWL_SOCKET = \"/old.sock\" } # sock\n\n# next\n[tui]\nx = 1\n";
    let new = codex(user, &with_madcawl(PEER_BIN));
    assert!(new.contains(&format!("env = {{ MADCAWL_SOCKET = \"{SOCKET}\" }} # sock\n")) && !new.contains("old.sock"), "{new}");
    assert!(new.contains("# mine\ndefault_tools_approval_mode = \"approve\"\n"), "{new}");
    assert!(new.contains("\n# next\n[tui]\nx = 1\n"), "{new}");
    // taken out in the middle of a file: the table after it and its comment stay
    let gone = codex(&new, &without_madcawl(PEER_BIN));
    assert!(!gone.contains("madcawl") && gone.starts_with("# next\n[tui]\nx = 1\n"), "{gone}");
    // pointed at a program of the user's own: theirs
    let elsewhere = "[mcp_servers.madcawl]\ncommand = \"/dev/madcawl-mcp\"\n";
    assert!(codex(elsewhere, &without_madcawl(PEER_BIN)).starts_with(elsewhere));
    // an environment table of its own cannot take a second `env`
    let sub = "[mcp_servers.madcawl]\ncommand = \"/u/.local/bin/madcawl-mcp\"\n[mcp_servers.madcawl.env]\nA = \"b\"\n";
    let (out, notes) = codex_config::with_table(sub, &with_madcawl(PEER_BIN));
    assert!(out.contains(sub) && notes.iter().any(|n| n.starts_with("madcawl: ")), "{notes:?}");
}

/// Only `MADCAWL_SOCKET` in an inline `env` is ours: the other pairs
/// and the comment after the table stay, a missing pair goes in before
/// the closing brace.
#[test]
fn mcp_table_codex_env_pair_is_set_alone() {
    let head = format!("[mcp_servers.madcawl]\ncommand = \"{MADCAWL_AT}\"\n");
    let tail = "default_tools_approval_mode = \"approve\"\n";
    let user = format!("{head}env = {{ MADCAWL_SOCKET = \"x\", RUST_LOG = \"debug\" }} # sock\n{tail}");
    let (out, notes) = codex_config::with_table(&user, &with_madcawl(PEER_BIN));
    let want = format!("{head}env = {{ MADCAWL_SOCKET = \"{SOCKET}\", RUST_LOG = \"debug\" }} # sock\n{tail}");
    assert!(out.starts_with(&want), "{out}");
    assert!(!notes.iter().any(|n| n.starts_with("madcawl: ")), "{notes:?}");
    let user = format!("{head}env = {{ RUST_LOG = \"debug\" }}\n{tail}");
    let out = codex(&user, &with_madcawl(PEER_BIN));
    assert!(out.starts_with(&format!("{head}env = {{ RUST_LOG = \"debug\", MADCAWL_SOCKET = \"{SOCKET}\" }}\n{tail}")), "{out}");
    // an environment written some other way is refused, not doubled
    for env in ["env = \"x\"\n", "env.MADCAWL_SOCKET = \"x\"\n", "\"env\".RUST_LOG = \"debug\"\n"] {
        let user = format!("{head}{env}{tail}");
        let (out, notes) = codex_config::with_table(&user, &with_madcawl(PEER_BIN));
        assert!(out.starts_with(&user) && out.matches("env").count() == user.matches("env").count(), "{out}");
        assert!(notes.iter().any(|n| n == &format!("madcawl: {}", codex_config::UNREADABLE)), "{notes:?}");
    }
}

/// A `madcawl` table whose command is not the fixed path is the user's:
/// with the program installed it is not repointed, its env not set,
/// and a note says so.  The peer table is still added.
#[test]
fn mcp_table_codex_madcawl_of_the_users_own_is_left_whole() {
    let user = "[mcp_servers.madcawl]\ncommand = \"/dev/madcawl-mcp\"\nargs = [\"--x\"]\n";
    let (out, notes) = codex_config::with_table(user, &with_madcawl(PEER_BIN));
    assert!(out.starts_with(user) && out.matches("madcawl").count() == 2, "{out}");
    assert!(out.contains("[mcp_servers.marspot]"), "{out}");
    assert!(notes.contains(&format!("madcawl: {}", super::mcp_servers::NOT_OURS)), "{notes:?}");
}

/// The JSON writers keep a `madcawl` entry of the user's own as it is,
/// and a file whose `madcawl` entry or `marspot.env` they would never
/// write still gets its `marspot` entry repointed.
#[test]
fn mcp_table_json_users_entries_are_kept_and_do_not_block_the_peer() {
    for (tag, name) in [("mcp-table-gemini-own", "settings.json"), ("mcp-table-kimi-own", "mcp.json")] {
        let write = |f: &PathBuf, t: &super::mcp_servers::Servers| match name {
            "settings.json" => gemini_config::reconcile_file(f, t),
            _ => kimi_config::reconcile_mcp(f, t),
        };
        let (_d, f) = tmp_file(tag, name);
        let user = r#"{"mcpServers":{"marspot":{"command":"/old","env":"x"},"madcawl":{"command":"/dev/m","args":["--x"]}}}"#;
        for t in [with_madcawl(PEER_BIN), without_madcawl(PEER_BIN)] {
            std::fs::write(&f, user).unwrap();
            let note = write(&f, &t).unwrap_or_default();
            let v = json::parse(&std::fs::read_to_string(&f).unwrap()).unwrap();
            assert_eq!(v.at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some(PEER_BIN), "{name}: {note}");
            assert_eq!(v.at(&["mcpServers", MADCAWL]), json::parse(user).unwrap().at(&["mcpServers", MADCAWL]), "{name}");
            assert_eq!(note.contains(super::mcp_servers::NOT_OURS), t.gone.is_empty(), "{name}: {note}");
        }
        std::fs::write(&f, r#"{"mcpServers":{"marspot":{"command":"/old"},"madcawl":"off"}}"#).unwrap();
        write(&f, &with_madcawl(PEER_BIN));
        let v = json::parse(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v.at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some(PEER_BIN), "{name}");
        assert_eq!(v.at(&["mcpServers", MADCAWL]).and_then(Value::str), Some("off"), "{name}");
    }
}

fn tmp_file(tag: &str, name: &str) -> (TempDir, PathBuf) {
    let d = TempDir::new(tag);
    let f = d.join(name);
    (d, f)
}

#[test]
fn mcp_table_gemini_and_kimi_json_get_madcawl_with_its_socket_and_lose_it() {
    for (tag, name) in [("mcp-table-gemini", "settings.json"), ("mcp-table-kimi", "mcp.json")] {
        let write = |f: &PathBuf, t: &super::mcp_servers::Servers| match name {
            "settings.json" => gemini_config::reconcile_file(f, t),
            _ => kimi_config::reconcile_mcp(f, t),
        };
        let (_d, old) = tmp_file(tag, "old");
        let (_e, f) = tmp_file(tag, name);
        write(&old, &of(PEER_BIN, None));
        write(&f, &without_madcawl(PEER_BIN));
        let before = std::fs::read_to_string(&f).unwrap();
        assert_eq!(before, std::fs::read_to_string(&old).unwrap(), "{name}: byte-identical without madcawl");

        assert!(write(&f, &with_madcawl(PEER_BIN)).is_some(), "{name}");
        let v = json::parse(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let m = v.at(&["mcpServers", MADCAWL]).unwrap();
        assert_eq!(m.str_at("command"), Some(MADCAWL_AT), "{name}");
        assert_eq!(m.get("args"), Some(&Value::Arr(Vec::new())), "{name}");
        assert_eq!(m.at(&["env", "MADCAWL_SOCKET"]).and_then(Value::str), Some(SOCKET), "{name}");
        assert!(m.get("toolTimeoutMs").is_none(), "{name}: kimi's peer deadline is the peer server's alone");
        assert_eq!(v.at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some(PEER_BIN));
        assert_eq!(write(&f, &with_madcawl(PEER_BIN)), None, "{name}: a second pass changes nothing");

        assert!(write(&f, &without_madcawl(PEER_BIN)).is_some(), "{name}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before, "{name}: taken out byte for byte");
    }
}

#[test]
fn mcp_table_claude_file_is_unchanged_without_madcawl_and_carries_it_when_present() {
    let old = "{\n  \"mcpServers\": {\n    \"marspot\": {\n      \"command\": \"/a/marspot-peer\",\n      \"args\": [\n        \"mcp\"\n      ]\n    }\n  }\n}\n";
    assert_eq!(claude_mcp::text(&of(PEER_BIN, None)), old);
    assert_eq!(claude_mcp::text(&without_madcawl(PEER_BIN)), old);
    let v = json::parse(&claude_mcp::text(&with_madcawl(PEER_BIN))).unwrap();
    assert_eq!(v.at(&["mcpServers", MADCAWL, "command"]).and_then(Value::str), Some(MADCAWL_AT));
    assert_eq!(v.at(&["mcpServers", MADCAWL, "env", "MADCAWL_SOCKET"]).and_then(Value::str), Some(SOCKET));
}

/// The registration claude panes get, end to end through the probe: a
/// program put at `~/.local/bin/madcawl-mcp` is in the file the next
/// pass writes, with a socket in the state root, and out again once
/// the program is removed.
#[test]
fn mcp_table_claude_registration_follows_the_installed_program() {
    let home = TempDir::new("mcp-table-claude-home");
    let state = TempDir::new("mcp-table-claude-state");
    let mut env = EnvGuard::new();
    env.set("HOME", home.path()).set("MARSPOT_STATE_DIR", state.path()).set("MARSPOT_PEER_BIN", PEER_BIN);
    let file = state.join("claude-mcp.json");
    let read = || json::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(claude_mcp::reconcile().len(), 1, "written once");
    assert!(read().at(&["mcpServers", MADCAWL]).is_none());

    let bin = home.join(".local/bin/madcawl-mcp");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    assert_eq!(claude_mcp::reconcile().len(), 1, "rewritten for the new server");
    let v = read();
    assert_eq!(v.at(&["mcpServers", MADCAWL, "command"]).and_then(Value::str), bin.to_str());
    let socket = state.join("madcawl.sock");
    assert_eq!(v.at(&["mcpServers", MADCAWL, "env", "MADCAWL_SOCKET"]).and_then(Value::str), socket.to_str());
    assert!(claude_mcp::reconcile().is_empty(), "nothing changed, nothing written");

    std::fs::remove_file(&bin).unwrap();
    claude_mcp::reconcile();
    assert!(read().at(&["mcpServers", MADCAWL]).is_none());
    assert_eq!(read().at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some(PEER_BIN));
}
