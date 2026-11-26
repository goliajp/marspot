//! Rules about which code may know what, checked on the sources.
//!
//! Only rules the tree already keeps are here; each one fails on the
//! first file or manifest that breaks it.

mod manifest_deps;

use manifest_deps::{linked_section, manifest_deps, resolve, workspace_section};
use std::path::{Path, PathBuf};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

const AGENT_NAMES: [&str; 4] = ["claude", "codex", "gemini", "kimi"];

/// Every file under `dir`, with its path; none when it does not exist.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries {
        let p = e.expect("a readable entry").path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The scheduler decides for every agent alike: no file of it names
/// one, in its path or in its text.  An agent's particulars reach it
/// through the adapter interface only.
#[test]
fn the_scheduler_names_no_agent() {
    let dirs = ["src/bin/marspot-shell/agentsvc/sched", "crates/marspot-agent/src/sched"];
    let mut files = Vec::new();
    for d in dirs {
        files_under(&root().join(d), &mut files);
    }
    assert!(!files.is_empty(), "no scheduler file was found under {dirs:?}");
    let mut found = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(root()).unwrap_or(f).to_string_lossy().into_owned();
        for name in AGENT_NAMES {
            if rel.to_ascii_lowercase().contains(name) {
                found.push(format!("{rel}: the path names {name}"));
            }
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for (i, line) in text.lines().enumerate() {
            let lower = line.to_ascii_lowercase();
            for name in AGENT_NAMES.iter().filter(|n| lower.contains(**n)) {
                found.push(format!("{rel}:{}: names {name}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "the scheduler names an agent:\n{}", found.join("\n"));
}

fn read_manifest(dir: &Path) -> String {
    let manifest = dir.join("Cargo.toml");
    std::fs::read_to_string(&manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()))
}

/// The crates a manifest pulls in by path for building, `.workspace =
/// true` entries resolved through the root manifest.
fn path_deps(crate_dir: &Path) -> Vec<(String, PathBuf)> {
    let ws = manifest_deps(&read_manifest(root()), workspace_section);
    resolve(manifest_deps(&read_manifest(crate_dir), linked_section), crate_dir, &ws, root())
}

/// Every crate `start` links by path, itself excluded.
fn path_closure(start: &[(String, PathBuf)]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut todo: Vec<(String, PathBuf)> = start.to_vec();
    while let Some((name, dir)) = todo.pop() {
        if seen.contains(&name) {
            continue;
        }
        todo.extend(path_deps(&dir));
        seen.push(name);
    }
    seen
}

const AGENT_CRATES: [&str; 2] = ["marspot-agent", "marspot-adapters"];

/// The session links neither the agent services nor the adapters.
#[test]
fn the_session_does_not_link_agent_crates() {
    let closure = path_closure(&path_deps(&root().join("crates/marspot-session")));
    assert!(closure.iter().any(|c| c == "marspot-term"), "the manifest read wrong: {closure:?}");
    for c in AGENT_CRATES {
        assert!(!closure.iter().any(|d| d == c), "marspot-session links {c}: {closure:?}");
    }
}

/// The core is a binary of the root package, which the shell shares, so
/// the root manifest names the agent crates; the core still links none
/// of them: no crate the root links besides them pulls one in, and no
/// source of the core or of the library names one.
#[test]
fn the_core_does_not_link_agent_crates() {
    let others: Vec<(String, PathBuf)> =
        path_deps(root()).into_iter().filter(|(n, _)| !AGENT_CRATES.contains(&n.as_str())).collect();
    assert!(others.iter().any(|(n, _)| n == "marspot-term"), "the root manifest read wrong: {others:?}");
    let closure = path_closure(&others);
    for c in AGENT_CRATES {
        assert!(!closure.iter().any(|d| d == c), "a crate the core links pulls in {c}: {closure:?}");
    }

    let mut files = Vec::new();
    files_under(&root().join("src"), &mut files);
    let core_or_lib = |p: &Path| {
        let rel = p.strip_prefix(root().join("src")).unwrap_or(p);
        !rel.starts_with("bin") || rel.starts_with("bin/marspot-core") || rel == Path::new("bin/marspot-core.rs")
    };
    let sources: Vec<&PathBuf> = files.iter().filter(|p| core_or_lib(p) && p.extension().is_some_and(|x| x == "rs")).collect();
    assert!(sources.len() > 100, "only {} core and library sources were found", sources.len());
    let mut found = Vec::new();
    for f in sources {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        for c in AGENT_CRATES {
            if text.contains(&c.replace('-', "_")) {
                found.push(format!("{}: names {c}", f.display()));
            }
        }
    }
    assert!(found.is_empty(), "the core or the library uses an agent crate:\n{}", found.join("\n"));
}
