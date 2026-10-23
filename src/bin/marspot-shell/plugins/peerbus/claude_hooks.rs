//! The Claude Code hooks that let a Claude session address peers by
//! marspot pane name (`marspot_peer::claude_hook`), kept in every
//! Claude Code settings file the way the status-line hook is.
//!
//! Two entries, both ours and nobody else's: before `SendMessage` and
//! after `ListAgents`.  Every other entry, and the rest of the file, is
//! left as it was.  The file is edited only when it is in the form
//! Claude Code itself writes (`JSON.stringify(…, null, 2)`), so the
//! edit changes nothing but our entries; a hand-formatted file is left
//! alone and logged.

use std::path::Path;

use marspot_peer::json::{self, Value, obj};
use marspot_term::lx_info;

impl super::worker::Worker {
    pub(super) fn reconcile_hooks(&self) {
        for note in reconcile() {
            lx_info!("peerbus.claude_hooks", &note);
        }
    }
}

/// The two hook entries: event, the tool it matches.
const HOOKS: &[(&str, &str)] = &[("PreToolUse", "SendMessage"), ("PostToolUse", "ListAgents")];

/// Install or repoint the hooks in every settings file.  Returns lines
/// worth logging.
pub(super) fn reconcile() -> Vec<String> {
    // a sandbox shares the developer's HOME, and these are the
    // developer's own settings
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return Vec::new();
    }
    let Some(bin) = super::inbox::peer_bin() else {
        return vec!["marspot-peer is not installed; no Claude Code hooks".into()];
    };
    let command = format!("{} claude-hook", crate::plugins::claudecode::sh_quote(&bin.to_string_lossy()));
    let mut notes = Vec::new();
    for path in crate::plugins::claudecode::cc_settings_files() {
        if let Some(note) = reconcile_file(&path, &command) {
            notes.push(format!("{}: {note}", path.display()));
        }
    }
    notes
}

fn reconcile_file(path: &Path, command: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut v = json::parse(&text).ok()?;
    if format!("{}\n", pretty(&v)) != text && pretty(&v) != text {
        return Some("not in the form Claude Code writes; hooks left out".into());
    }
    if !with_hooks(&mut v, command) {
        return None;
    }
    let mut new_text = pretty(&v);
    if text.ends_with('\n') {
        new_text.push('\n');
    }
    match crate::plugins::claudecode::write_through_symlink(path, &new_text) {
        Ok(()) => Some("pane-name hooks installed".into()),
        Err(e) => Some(e.to_string()),
    }
}

/// Our entry for `tool` under `event` in `v`, added or repointed at
/// `command`.  True when anything changed.
fn with_hooks(v: &mut Value, command: &str) -> bool {
    let mut changed = false;
    let hooks = slot(v, "hooks", || obj([]));
    for (event, tool) in HOOKS {
        let list = slot(hooks, event, || Value::Arr(Vec::new()));
        let Value::Arr(entries) = list else { continue };
        let entry = obj([
            ("matcher", Value::from(*tool)),
            ("hooks", Value::Arr(vec![obj([("type", Value::from("command")), ("command", Value::from(command))])])),
        ]);
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
        .any(|c| c.contains("marspot-peer") && c.contains("claude-hook"))
}

/// The text `JSON.stringify(value, null, 2)` gives, which is how
/// Claude Code writes its settings: a file in that form reads back
/// byte for byte after an edit made through this.
fn pretty(v: &Value) -> String {
    let mut out = String::new();
    write_pretty(v, &mut out, 0);
    out
}

fn write_pretty(v: &Value, out: &mut String, depth: usize) {
    let pad = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(d));
    };
    match v {
        Value::Arr(a) if !a.is_empty() => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                write_pretty(x, out, depth + 1);
            }
            pad(out, depth);
            out.push(']');
        }
        Value::Obj(kv) if !kv.is_empty() => {
            out.push('{');
            for (i, (k, x)) in kv.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, depth + 1);
                json::write_str(out, k);
                out.push_str(": ");
                write_pretty(x, out, depth + 1);
            }
            pad(out, depth);
            out.push('}');
        }
        other => other.write(out),
    }
}

/// The value at `key` of object `v`, created with `make` when missing.
fn slot<'a>(v: &'a mut Value, key: &str, make: impl FnOnce() -> Value) -> &'a mut Value {
    if v.get(key).is_none() {
        v.set(key, make());
    }
    let Value::Obj(kv) = v else { unreachable!("settings are an object") };
    &mut kv.iter_mut().find(|(k, _)| k == key).expect("set above").1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SETTINGS: &str = "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"/x/zsh-colon-guard\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-hooks-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The layout `JSON.stringify(v, null, 2)` produces, empty
    /// containers and nesting included.
    #[test]
    fn pretty_matches_json_stringify() {
        let text = "{\n  \"a\": 1,\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": []\n      }\n    ]\n  },\n  \"e\": {},\n  \"s\": \"é \\\"q\\\"\"\n}";
        assert_eq!(pretty(&json::parse(text).unwrap()), text);
    }

    #[test]
    fn hooks_are_added_beside_other_peoples_and_repointed() {
        let d = dir();
        let f = d.join("settings.json");
        std::fs::write(&f, SETTINGS).unwrap();
        assert_eq!(reconcile_file(&f, "'/a/marspot-peer' claude-hook").as_deref(), Some("pane-name hooks installed"));
        let text = std::fs::read_to_string(&f).unwrap();
        let v = json::parse(&text).unwrap();
        let pre = v.at(&["hooks", "PreToolUse"]).unwrap().arr();
        assert_eq!(pre.len(), 2, "the other hook stays");
        assert_eq!(pre[0].str_at("matcher"), Some("Bash"));
        assert_eq!(pre[1].str_at("matcher"), Some("SendMessage"));
        assert_eq!(v.at(&["hooks", "PostToolUse"]).unwrap().arr()[0].str_at("matcher"), Some("ListAgents"));
        assert!(text.starts_with("{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\""));
        assert!(text.ends_with("}\n"));
        // nothing to do the second time
        assert_eq!(reconcile_file(&f, "'/a/marspot-peer' claude-hook"), None);
        // a new path repoints our entries and adds none
        reconcile_file(&f, "'/b/marspot-peer' claude-hook");
        let v = json::parse(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let pre = v.at(&["hooks", "PreToolUse"]).unwrap().arr();
        assert_eq!(pre.len(), 2);
        assert!(pre[1].to_json().contains("/b/marspot-peer"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_hand_formatted_file_is_left_alone() {
        let d = dir();
        let f = d.join("settings.json");
        let hand = "{ \"model\": \"opus\" }\n";
        std::fs::write(&f, hand).unwrap();
        assert!(reconcile_file(&f, "x claude-hook marspot-peer").unwrap().contains("left out"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), hand);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
