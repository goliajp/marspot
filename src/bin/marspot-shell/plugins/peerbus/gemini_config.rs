//! What a Gemini session needs in its `settings.json` to be a peer,
//! kept in every Gemini home the way the Claude Code hooks are kept in
//! its settings: the MCP server that gives it the peer tools
//! (`mcpServers.marspot`, running `marspot-peer mcp`), and the hook
//! that hands it the messages waiting for it (`marspot-peer
//! gemini-hook`, before each prompt and after each tool call).
//!
//! Those entries are ours and nobody else's.  Every other server and
//! hook, and the rest of the file, is left as it was, value for value;
//! a key the user added to our server entry stays too.  The file is
//! written in `JSON.stringify(…, null, 2)` form.  A file that does not
//! parse as plain JSON — one with comments, which Gemini itself
//! accepts — is never written, and that home's sessions are then
//! without the tools until its owner adds them.

use std::path::{Path, PathBuf};

use marspot_peer::gemini_hook::EVENTS;
use marspot_peer::json::{self, Value, obj};

use super::claude_hooks::{pretty, slot};

const SERVER: &str = "marspot";
/// What the entry is called in Gemini's `/hooks` panel.
const HOOK_NAME: &str = "marspot-peer";

/// Add or repoint the entries in every Gemini home.  Returns lines
/// worth logging.
pub(super) fn reconcile() -> Vec<String> {
    // a sandbox shares the developer's HOME, and these are the
    // developer's own Gemini homes
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return Vec::new();
    }
    let (Some(bin), Some(home)) = (super::inbox::peer_bin(), std::env::var_os("HOME")) else {
        return Vec::new();
    };
    let Some(bin) = bin.to_str() else { return Vec::new() };
    settings_files(Path::new(&home))
        .iter()
        .filter_map(|f| reconcile_file(f, bin).map(|note| format!("{}: {note}", f.display())))
        .collect()
}

/// `settings.json` of `~/.gemini` and of every `~/.gemini-profile-N/.gemini`
/// that exists, each real file once: profiles that link to one shared
/// file would otherwise have it edited once per name.
pub(super) fn settings_files(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.to_path_buf()];
    roots.extend(crate::plugins::gemini::profile_dirs_in(home).into_iter().map(|(_, dir)| dir));
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        let dir = crate::plugins::gemini::config_dir(&root);
        // a config directory that does not exist is not created
        let Ok(real_dir) = dir.canonicalize() else { continue };
        if !real_dir.is_dir() {
            continue;
        }
        let file = dir.join("settings.json");
        let real = match file.canonicalize() {
            Ok(real) => real,
            Err(_) if std::fs::symlink_metadata(&file).is_err() => real_dir.join("settings.json"),
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
        Ok(text) => text,
        // a home gemini has not written settings in yet
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}\n".to_string(),
        Err(_) => return None,
    };
    let Ok(mut v) = json::parse(&text) else {
        return Some("not plain JSON; the marspot entries are left out".into());
    };
    if !fits(&v) {
        return Some("not the shape of Gemini settings; the marspot entries are left out".into());
    }
    if !with_entries(&mut v, bin) {
        return None;
    }
    let mut new_text = pretty(&v);
    if text.ends_with('\n') {
        new_text.push('\n');
    }
    match crate::plugins::claudecode::write_through_symlink(path, &new_text) {
        Ok(()) => Some("peer tools and inbox hook set".into()),
        Err(e) => Some(e.to_string()),
    }
}

/// Whether everything `with_entries` steps through is an object, or a
/// list where Gemini keeps a list, or absent.
fn fits(v: &Value) -> bool {
    let obj_or_absent = |x: Option<&Value>| x.is_none_or(|x| matches!(x, Value::Obj(_)));
    matches!(v, Value::Obj(_))
        && obj_or_absent(v.get("mcpServers"))
        && obj_or_absent(v.at(&["mcpServers", SERVER]))
        && obj_or_absent(v.get("hooks"))
        && EVENTS.iter().all(|event| v.at(&["hooks", *event]).is_none_or(|l| matches!(l, Value::Arr(_))))
}

/// Our server and hooks in `v`, added or repointed at `bin`.  True when
/// anything changed.
fn with_entries(v: &mut Value, bin: &str) -> bool {
    let mut changed = false;
    let server = slot(slot(v, "mcpServers", || obj([])), SERVER, || obj([]));
    let args = Value::Arr(vec![Value::from("mcp")]);
    if server.str_at("command") != Some(bin) {
        server.set("command", Value::from(bin));
        changed = true;
    }
    if server.get("args") != Some(&args) {
        server.set("args", args);
        changed = true;
    }

    // gemini runs a hook's command through the shell
    let command = format!("{} gemini-hook", crate::plugins::claudecode::sh_quote(bin));
    let hooks = slot(v, "hooks", || obj([]));
    for event in EVENTS {
        let Value::Arr(entries) = slot(hooks, event, || Value::Arr(Vec::new())) else { continue };
        let entry = obj([(
            "hooks",
            Value::Arr(vec![obj([
                ("name", Value::from(HOOK_NAME)),
                ("type", Value::from("command")),
                ("command", Value::from(command.as_str())),
            ])]),
        )]);
        match entries.iter_mut().find(|e| is_ours(e)) {
            Some(e) if *e == entry => {}
            Some(e) => {
                *e = entry;
                changed = true;
            }
            None => {
                entries.push(entry);
                changed = true;
            }
        }
    }
    changed
}

/// An entry whose commands are ours.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .map_or(&[][..], Value::arr)
        .iter()
        .filter_map(|h| h.str_at("command"))
        .any(|c| c.contains("marspot-peer") && c.contains("gemini-hook"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHERS: &str = "{\n  \"general\": {\n    \"enableAutoUpdate\": false\n  },\n  \"mcpServers\": {\n    \"files\": {\n      \"command\": \"node\",\n      \"args\": [\n        \"server.js\"\n      ]\n    }\n  },\n  \"hooks\": {\n    \"BeforeAgent\": [\n      {\n        \"matcher\": \"*\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"/x/log-prompt.sh\"\n          }\n        ]\n      }\n    ],\n    \"SessionEnd\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"say bye\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-gemini-cfg-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn read(f: &Path) -> Value {
        json::parse(&std::fs::read_to_string(f).unwrap()).unwrap()
    }

    fn assert_ours(v: &Value, bin: &str) {
        assert_eq!(v.at(&["mcpServers", "marspot", "command"]).and_then(Value::str), Some(bin));
        assert_eq!(v.at(&["mcpServers", "marspot", "args"]).unwrap().to_json(), "[\"mcp\"]");
        for event in ["BeforeAgent", "AfterTool"] {
            let ours: Vec<&Value> = v.at(&["hooks", event]).unwrap().arr().iter().filter(|e| is_ours(e)).collect();
            assert_eq!(ours.len(), 1, "{event}");
            let hook = &ours[0].get("hooks").unwrap().arr()[0];
            assert_eq!(hook.str_at("type"), Some("command"));
            assert_eq!(hook.str_at("command"), Some(format!("'{bin}' gemini-hook").as_str()));
        }
    }

    #[test]
    fn a_home_with_no_settings_file_gets_one() {
        let f = dir().join("settings.json");
        assert_eq!(reconcile_file(&f, "/a/marspot-peer").as_deref(), Some("peer tools and inbox hook set"));
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.starts_with("{\n  \"mcpServers\": {\n    \"marspot\": {\n      \"command\": \"/a/marspot-peer\",\n      \"args\": [\n        \"mcp\"\n      ]\n    }\n  },\n  \"hooks\": {"), "{text}");
        assert!(text.ends_with("}\n"));
        assert_ours(&read(&f), "/a/marspot-peer");
    }

    #[test]
    fn an_empty_object_gets_the_entries() {
        let f = dir().join("settings.json");
        std::fs::write(&f, "{}").unwrap();
        assert!(reconcile_file(&f, "/a/marspot-peer").is_some());
        assert_ours(&read(&f), "/a/marspot-peer");
        assert!(!std::fs::read_to_string(&f).unwrap().ends_with('\n'), "no newline was there, none is added");
    }

    #[test]
    fn other_servers_and_hooks_stay_and_a_second_run_changes_nothing() {
        let f = dir().join("settings.json");
        std::fs::write(&f, OTHERS).unwrap();
        let before = json::parse(OTHERS).unwrap();
        assert!(reconcile_file(&f, "/a/marspot-peer").is_some());
        let first = std::fs::read_to_string(&f).unwrap();
        let v = read(&f);
        assert_ours(&v, "/a/marspot-peer");
        assert_eq!(v.get("general"), before.get("general"));
        assert_eq!(v.at(&["mcpServers", "files"]), before.at(&["mcpServers", "files"]));
        assert_eq!(v.at(&["hooks", "SessionEnd"]), before.at(&["hooks", "SessionEnd"]));
        let prompt = v.at(&["hooks", "BeforeAgent"]).unwrap().arr();
        assert_eq!(prompt.len(), 2, "the other hook of the same event stays");
        assert_eq!(Some(&prompt[0]), before.at(&["hooks", "BeforeAgent"]).map(|l| &l.arr()[0]));
        // everything that was there is still where it was, byte for byte
        assert!(first.starts_with("{\n  \"general\": {\n    \"enableAutoUpdate\": false\n  },\n  \"mcpServers\": {\n    \"files\": {"), "{first}");
        assert_eq!(reconcile_file(&f, "/a/marspot-peer"), None);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), first, "byte-identical after the second run");
    }

    #[test]
    fn a_moved_helper_repoints_our_entries_and_keeps_what_the_user_added_to_them() {
        let f = dir().join("settings.json");
        std::fs::write(&f, OTHERS).unwrap();
        reconcile_file(&f, "/a/marspot-peer");
        // the user trusts the server so its tools run unprompted
        let mut v = read(&f);
        slot(slot(&mut v, "mcpServers", || obj([])), "marspot", || obj([])).set("trust", Value::from(true));
        std::fs::write(&f, pretty(&v)).unwrap();
        assert!(reconcile_file(&f, "/b/marspot-peer").is_some());
        let v = read(&f);
        assert_ours(&v, "/b/marspot-peer");
        assert_eq!(v.at(&["mcpServers", "marspot", "trust"]).and_then(Value::bool), Some(true));
        assert_eq!(v.at(&["hooks", "BeforeAgent"]).unwrap().arr().len(), 2, "repointed, not added again");
        assert_eq!(v.at(&["hooks", "AfterTool"]).unwrap().arr().len(), 1);
        assert!(!std::fs::read_to_string(&f).unwrap().contains("/a/marspot-peer"));
    }

    #[test]
    fn a_file_that_does_not_parse_or_fit_is_never_written() {
        let f = dir().join("settings.json");
        for odd in [
            "{ \"general\": ",
            "// my settings\n{\"general\": {}}\n",
            "[1, 2]\n",
            "{\"mcpServers\": []}",
            "{\"mcpServers\": {\"marspot\": \"off\"}}",
            "{\"hooks\": []}",
            "{\"hooks\": {\"AfterTool\": {}}}",
        ] {
            std::fs::write(&f, odd).unwrap();
            assert!(reconcile_file(&f, "/a/marspot-peer").is_some_and(|note| note.contains("left out")), "{odd}");
            assert_eq!(std::fs::read_to_string(&f).unwrap(), odd);
        }
    }

    /// The default home and two profiles, the profiles sharing one
    /// settings file through a link: two real files, each once.
    #[test]
    fn a_shared_settings_file_is_listed_once_and_edited_through_the_link() {
        let home = dir();
        let shared = home.join("shared-settings.json");
        std::fs::write(&shared, "{}\n").unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        for n in [1, 2] {
            let cfg = home.join(format!(".gemini-profile-{n}/.gemini"));
            std::fs::create_dir_all(&cfg).unwrap();
            std::os::unix::fs::symlink(&shared, cfg.join("settings.json")).unwrap();
        }
        // a profile gemini never ran in, and one whose link is dead
        std::fs::create_dir_all(home.join(".gemini-profile-3")).unwrap();
        std::fs::create_dir_all(home.join(".gemini-profile-4/.gemini")).unwrap();
        std::os::unix::fs::symlink(home.join("nowhere.json"), home.join(".gemini-profile-4/.gemini/settings.json")).unwrap();

        let files = settings_files(&home);
        assert_eq!(files, vec![home.join(".gemini/settings.json"), shared.clone()]);
        for f in &files {
            assert!(reconcile_file(f, "/a/marspot-peer").is_some());
        }
        assert_ours(&read(&shared), "/a/marspot-peer");
        assert_ours(&read(&home.join(".gemini/settings.json")), "/a/marspot-peer");
        let link = home.join(".gemini-profile-1/.gemini/settings.json");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is still a link");
        assert_ours(&read(&link), "/a/marspot-peer");
        assert!(!home.join(".gemini-profile-3/.gemini").exists(), "no config directory is created");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
