//! The paths the source builds on the state root, read out of the source
//! itself so a path no run happened to create is checked too.
//!
//! A path starts at `state_root()`, `sessions_dir()`, `session_dir(..)`,
//! `binaries_root()`, `log_dir()`, `StateStore::open("..")`, or a
//! plugin's `state_dir()` (`plugins/<plugin>/`).  It is followed
//! through `let`s, through `.join`, `.push`, `.with_extension` and
//! `.with_file_name` of a string literal, a `&str` constant, a
//! `format!` or a number's `to_string()`, through `OsString` appends,
//! and through the functions and closures it is passed to or returned
//! from.  A part the source computes is a hole that any placeholder of
//! the state map may fill.  Production code only: test files and the
//! items under `#[cfg(test)]` are left out, as test code builds paths
//! in temporary directories.

mod eval;
mod lex;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;

use eval::{HOLE, Scan, Src};

/// Each scanned production source: its path and its text without
/// comments and test items, its lines joined by spaces.
pub(super) fn sources(root: &Path) -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).expect("a readable source tree") {
            let p = e.expect("a readable entry").path();
            let name = p.to_string_lossy().into_owned();
            if p.is_dir() {
                if !name.ends_with("/tests") {
                    walk(&p, out);
                }
            } else if name.ends_with(".rs") && !is_test_file(&name) {
                let text = std::fs::read_to_string(&p).expect("a readable source file");
                out.push((name, prepare(&text)));
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("src"), &mut out);
    for c in std::fs::read_dir(root.join("crates")).expect("the crates directory") {
        let src = c.expect("a readable entry").path().join("src");
        if src.is_dir() {
            walk(&src, &mut out);
        }
    }
    out.sort();
    out
}

fn is_test_file(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    file == "tests.rs" || file.ends_with("_tests.rs") || file.starts_with("tests_") || file == "test_env.rs"
}

pub(super) fn prepare(text: &str) -> String {
    let code = lex::without_tests(&lex::strip_comments(text));
    code.lines().map(str::trim).collect::<Vec<_>>().join(" ")
}

/// `const NAME: &str = "value";` (or `&'static str`) of every source,
/// by name; a name with more than one value keeps the value of each
/// file it is in.
pub(super) fn constants(sources: &[(String, String)]) -> HashMap<String, Vec<(String, String)>> {
    let mut out: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (file, text) in sources {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("const ") {
            rest = &rest[at + 6..];
            let Some((name, tail)) = rest.split_once(':') else { continue };
            let tail = tail.trim_start();
            let Some(tail) = tail.strip_prefix("&str").or_else(|| tail.strip_prefix("&'static str")) else { continue };
            let Some(lit) = tail.trim_start().strip_prefix('=').map(str::trim_start).filter(|t| t.starts_with('"')) else { continue };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
                continue;
            }
            if let Some(end) = lex::literal_end(lit.as_bytes(), 0) {
                out.entry(name.to_string()).or_default().push((file.clone(), lit[1..end - 1].to_string()));
            }
        }
    }
    out
}

/// One path found: the file it is built in and the path relative to the
/// state root, [`HOLE`] standing for each computed part.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Found {
    pub file: String,
    pub path: String,
}

impl Found {
    /// The path with each hole shown as `{}`.
    pub(super) fn shown(&self) -> String {
        self.path.replace(HOLE, "{}")
    }
}

/// Every path the sources build on the state root.
pub(super) fn paths(sources: &[(String, String)]) -> Vec<Found> {
    let consts = constants(sources);
    let srcs: Vec<Src> = sources.iter().map(|(f, t)| Src { file: f.clone(), fns: lex::functions(t), text: t.clone() }).collect();
    let mut scan = Scan::new(&srcs, &consts);
    // the second pass reads the struct fields the first one saw set
    for _ in 0..2 {
        for (si, s) in srcs.iter().enumerate() {
            scan.walk(si, 0..s.text.len(), &mut HashMap::new());
        }
        scan.next_pass();
    }
    scan.found.into_iter().map(|(path, file)| Found { file, path }).collect()
}

/// What a hole may be: a number, a date, a uuid, a name.
const HOLE_FILLS: &[&str] = &["1", "20261126", "2fec8734-bf76-478b-8044-20261126b7e4", "x"];

/// Whether the state map knows `path`, for some filling of its holes.
pub(super) fn registered(path: &str) -> bool {
    if !path.contains(HOLE) {
        return super::is_registered(path) || is_tmp_of_registered(path);
    }
    HOLE_FILLS.iter().map(|fill| path.replace(HOLE, fill)).any(|p| super::is_registered(&p) || is_tmp_of_registered(&p))
}

/// The temporary half of an atomic replace (`f.tmp`, `f.<ext>.tmp`,
/// `.f.tmp` beside `f.<ext>`) is known with the file it replaces, by the
/// catch-all `**/*.tmp` row.
fn is_tmp_of_registered(path: &str) -> bool {
    if !path.ends_with(".tmp") {
        return false;
    }
    super::STATE_MAP.iter().filter(|e| !e.path.starts_with("**")).any(|e| {
        let (dir, name) = e.path.rsplit_once('/').map_or(("", e.path), |(d, n)| (d, n));
        let dotted = if dir.is_empty() { format!(".{name}.tmp") } else { format!("{dir}/.{name}.tmp") };
        [format!("{}.tmp", e.path), eval::with_extension(e.path, "tmp"), dotted].iter().any(|t| super::pattern::matches(t, path))
    })
}

#[test]
fn state_paths_in_source_are_registered() {
    let srcs = sources(Path::new(env!("CARGO_MANIFEST_DIR")));
    assert!(srcs.len() > 300, "read {} sources, so this test is looking in the wrong place", srcs.len());
    let found = paths(&srcs);
    assert!(found.len() > 50, "found {} paths, so the scan no longer reads the source's shapes", found.len());
    for f in &found {
        println!("{}\t{}", f.shown(), f.file);
    }
    let missing: Vec<String> = found.iter().filter(|f| !registered(&f.path)).map(|f| format!("{} (built in {})", f.shown(), f.file)).collect();
    assert!(missing.is_empty(), "paths under the state root that host::state_map does not know:\n  {}", missing.join("\n  "));
    println!("{} paths built on the state root, all registered", found.len());
}
