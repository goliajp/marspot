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
    assert_eq!(text, format!("{CONFIG}\n[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n"));
    assert_eq!(reconcile_file(&f, "/a/marspot-peer"), None);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), text, "byte for byte");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_missing_file_is_created_and_one_without_a_final_newline_gains_it() {
    let d = dir();
    let f = d.join("config.toml");
    reconcile_file(&f, "/a/marspot-peer");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n");
    let new = with_server("model = \"gpt-5\"", "/a/marspot-peer").unwrap().unwrap();
    assert_eq!(new, "model = \"gpt-5\"\n\n[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn another_path_is_repointed_and_nothing_else_moves() {
    let old = "a = 1\n[ mcp_servers.\"marspot\" ] # peers\n  command = '/old/marspot-peer'   # where\nargs = [\"mcp\", \"-v\"]\n[mcp_servers.marspot.env]\ncommand = \"not this one\"\n[tui]\nx = 1";
    let new = with_server(old, "/new dir/marspot-peer").unwrap().unwrap();
    let said = "# where\ndefault_tools_approval_mode = \"approve\"\n";
    assert_eq!(new, old.replace("'/old/marspot-peer'", "\"/new dir/marspot-peer\"").replace("# where\n", said));
    assert_eq!(with_server(&new, "/new dir/marspot-peer"), Ok(None));
    // a literal string naming the same path is the same path
    assert_eq!(with_server(old, "/old/marspot-peer"), Ok(Some(old.replace("# where\n", said))));
    // quotes and backslashes in a path survive the round trip
    let odd = with_server("", "/a \"b\"\\c/marspot-peer").unwrap().unwrap();
    assert_eq!(with_server(&odd, "/a \"b\"\\c/marspot-peer"), Ok(None));
}

#[test]
fn a_table_header_inside_a_string_is_not_a_table() {
    let text = "notes = \"\"\"\n[mcp_servers.marspot]\ncommand = \"x\"\n\"\"\"\n";
    let new = with_server(text, "/a/marspot-peer").unwrap().unwrap();
    assert!(new.starts_with(text) && new.ends_with("[mcp_servers.marspot]\ncommand = \"/a/marspot-peer\"\nargs = [\"mcp\"]\ndefault_tools_approval_mode = \"approve\"\n"));
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

/// Each of these was read by codex 0.171 (`codex mcp get marspot`):
/// the mode alone where there is an entry, all three where there is
/// none, and never a command over an entry that may have a url.
#[test]
fn approval_goes_with_whatever_entry_the_config_has() {
    let mode = ["-c", "mcp_servers.marspot.default_tools_approval_mode=\"approve\""];
    let whole = [mode[0], mode[1], "-c", "mcp_servers.marspot.command=\"/a b/marspot-peer\"", "-c", "mcp_servers.marspot.args=[\"mcp\"]"];
    let bin = Some("/a b/marspot-peer");
    for text in ["", CONFIG, "[mcp_servers.other]\ncommand = \"/o\"\n"] {
        assert_eq!(approved(text, bin), whole, "{text}");
        assert!(approved(text, None).is_empty(), "nothing to run: {text}");
    }
    for text in [
        "[mcp_servers.marspot]\ncommand = \"/x/marspot-peer\"\nargs = [\"mcp\", \"-v\"]\n",
        "[mcp_servers.marspot]\ncommand = \"\"\n",
        "[mcp_servers.marspot]\ncommand = \"/x\"\ndefault_tools_approval_mode = \"prompt\"\n",
        "[mcp_servers.marspot]\ncommand = \"/x\"\ndefault_tools_approval_mode = 1\n",
        "[mcp_servers]\nmarspot = { command = \"/x\", args = [\"mcp\"] }\n",
        "[mcp_servers.marspot]\nurl = \"http://127.0.0.1:1/mcp\"\n",
    ] {
        assert_eq!(approved(text, bin), mode, "{text}");
    }
    assert!(approved("notes = \"\"\"\nno end", bin).is_empty(), "not known what is there");
    let d = dir();
    std::fs::write(d.join("config.toml"), with_server(CONFIG, "/x/marspot-peer").unwrap().unwrap()).unwrap();
    assert!(approved_args(&d).is_empty(), "the file says it: nothing on the command line");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_mode_the_user_set_is_kept() {
    for mode in ["\"prompt\"", "'auto' # ask", "\"approve\"", "1"] {
        let text = format!("[mcp_servers.marspot]\ncommand = \"/a\"\n\"default_tools_approval_mode\" = {mode}\n");
        assert_eq!(with_server(&text, "/a"), Ok(None), "{mode}");
        assert_eq!(with_server(&text, "/b"), Ok(Some(text.replace("\"/a\"", "\"/b\""))), "{mode}");
    }
    // another table's is not the server's
    let other = "[mcp_servers.marspot]\ncommand = \"/a\"\n[mcp_servers.o]\ndefault_tools_approval_mode = \"prompt\"\n";
    assert_eq!(with_server(other, "/a").unwrap().unwrap(), other.replace("\"/a\"\n", "\"/a\"\ndefault_tools_approval_mode = \"approve\"\n"));
    let last = "[mcp_servers.marspot]\ncommand = \"/a\"";
    assert_eq!(with_server(last, "/a").unwrap().unwrap(), format!("{last}\ndefault_tools_approval_mode = \"approve\"\n"));
}
