//! The exec exit: its hooks run around a real exec, and every place the
//! shell replaces its image goes through it.

use std::path::{Path, PathBuf};

use super::*;

static TRACE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn push(d: u32) {
    TRACE.lock().unwrap().push(d);
}

#[test]
fn exec_hooks_run_in_registration_order_and_again_when_the_exec_fails() {
    register(ExecHook { name: "test-first", before: || push(1), failed: Some(|| push(3)) });
    register(ExecHook { name: "test-second", before: || push(2), failed: None });
    // registered again: replaced in place, not moved to the end
    register(ExecHook { name: "test-first", before: || push(1), failed: Some(|| push(4)) });
    let names = names();
    let at = |n| names.iter().position(|x| *x == n).unwrap();
    assert!(at("test-first") < at("test-second"), "{names:?}");
    assert_eq!(names.iter().filter(|n| **n == "test-first").count(), 1);

    // a real exec, of a file that is not there: it returns
    let err = exec_prepared(ExecKind::ShellUpdate, &mut std::process::Command::new("/nonexistent/marspot-shell"));
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    assert_eq!(*TRACE.lock().unwrap(), [1, 2, 4], "before hooks in order, then the failed one");
}

/// Shell sources outside tests, with their paths relative to the shell's
/// directory.
fn production_sources() -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                if name != "tests" {
                    walk(root, &p, out);
                }
            } else if name.ends_with(".rs") && !name.contains("tests") {
                let text = std::fs::read_to_string(&p).unwrap();
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((rel, without_test_module(&text).to_string()));
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/bin/marspot-shell");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    assert!(out.len() > 100, "found {} shell sources: looking in the wrong place", out.len());
    out
}

/// The text before a trailing `#[cfg(test)] mod …` block.
fn without_test_module(text: &str) -> &str {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let rest = &text[at + line.len()..];
        if line.trim_start().starts_with("#[cfg(test)]") && rest.trim_start().starts_with("mod ") {
            return &text[..at];
        }
        at += line.len();
    }
    text
}

#[test]
fn every_exec_goes_through_exec_prepared() {
    let sources = production_sources();
    let execs: Vec<&str> = sources
        .iter()
        .filter(|(_, t)| t.contains(".exec()") || t.contains("libc::execv"))
        .map(|(p, _)| p.as_str())
        .collect();
    assert_eq!(execs, ["platform/exec_hooks.rs"], "an exec outside the exit");
    let exit = &sources.iter().find(|(p, _)| p == "platform/exec_hooks.rs").unwrap().1;
    assert_eq!(exit.matches(".exec()").count(), 1);

    // the sites, each with how many times it replaces the image
    for (file, n) in [("self_exec.rs", 2), ("identity.rs", 1), ("resident/claim.rs", 1), ("launch.rs", 1)] {
        let text = &sources.iter().find(|(p, _)| p == file).unwrap_or_else(|| panic!("{file} moved")).1;
        assert_eq!(text.matches("exec_hooks::exec_prepared(").count(), n, "{file}");
    }
    let self_exec = &sources.iter().find(|(p, _)| p == "self_exec.rs").unwrap().1;
    for owner in ["plugins::", "agentsvc::"] {
        assert!(!self_exec.contains(owner), "self_exec.rs names {owner}");
    }
}

#[test]
fn exec_hooks_the_table_knows_no_service_or_plugin() {
    let src = include_str!("exec_hooks.rs");
    for word in ["Service", "Plugin", "plugins::", "agentsvc::"] {
        assert!(!src.contains(word), "exec_hooks.rs names {word}");
    }
}
