//! The paths the source builds on the state root, read out of the source
//! itself so a path no run happened to create is checked too.
//!
//! A path starts at `state_root()`, `sessions_dir()`, `session_dir(..)`,
//! `binaries_root()`, `log_dir()`, `StateStore::open(..)`, or a plugin's
//! `state_dir()` (`plugins/<plugin>/`), directly or through a `let` that
//! names it, and goes on through `.join(..)` of a string literal or of a
//! `&str` constant; it ends at the first argument that is neither.
//! Production code only: test files and trailing test modules are left
//! out, as test code builds paths in temporary directories.

use std::collections::HashMap;
use std::path::Path;

/// Each scanned production source: its path and its text with every
/// line break and the indentation after it removed, so a chain of calls
/// over several lines reads as one.
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
                out.push((name, flat(production(&text))));
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
    out
}

fn is_test_file(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    file == "tests.rs" || file.ends_with("_tests.rs") || file.starts_with("tests_") || file == "test_env.rs"
}

/// The text before a trailing `#[cfg(test)] mod ...`.
fn production(text: &str) -> &str {
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

fn flat(text: &str) -> String {
    text.lines().map(|l| l.trim()).filter(|l| !l.starts_with("//")).collect()
}

/// `const NAME: &str = "value";` of every source, by name; a name with
/// more than one value keeps the value of each file it is in.
pub(super) fn constants(sources: &[(String, String)]) -> HashMap<String, Vec<(String, String)>> {
    let mut out: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (file, text) in sources {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("const ") {
            rest = &rest[at + 6..];
            let Some((name, tail)) = rest.split_once(": &str = \"") else { continue };
            if !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') || name.is_empty() {
                continue;
            }
            if let Some(value) = literal_body(tail) {
                out.entry(name.to_string()).or_default().push((file.clone(), value.to_string()));
            }
        }
    }
    out
}

/// The text up to the closing quote of a string literal whose opening
/// quote was just passed.
fn literal_body(s: &str) -> Option<&str> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            '\\' => escaped = !escaped,
            '"' if !escaped => return Some(&s[..i]),
            _ => escaped = false,
        }
    }
    None
}

/// One path found: the file it is built in and the path relative to the
/// state root, `<id>` standing for a session id.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Found {
    pub file: String,
    pub path: String,
}

const BASES: &[(&str, &str)] = &[
    ("state_root()", ""),
    ("sessions_dir()", "sessions"),
    ("binaries_root()", "binaries"),
    ("log_dir()", "logs"),
];

/// Every path the sources build on the state root.
pub(super) fn paths(sources: &[(String, String)]) -> Vec<Found> {
    let consts = constants(sources);
    let mut out = Vec::new();
    for (file, text) in sources {
        let plugin = file.split("/plugins/").nth(1).and_then(|r| r.split_once('/')).map(|(p, _)| p);
        let mut starts: Vec<(usize, String)> = Vec::new();
        for (base, prefix) in BASES {
            starts.extend(words(text, base).map(|(at, b)| (at + b.len(), prefix.to_string())));
        }
        for (at, _) in words(text, "session_dir(") {
            // the registry's, of one session id; other modules have their own
            if let Some(close) = text[at..].find(')').filter(|c| !text[at..at + c].contains(',')) {
                starts.push((at + close + 1, "sessions/1".into()));
            }
        }
        for (at, m) in text.match_indices("StateStore::open(\"") {
            if let Some(name) = literal_body(&text[at + m.len()..]) {
                starts.push((at + m.len() + name.len() + 2, name.to_string()));
            }
        }
        if let Some(p) = plugin {
            starts.extend(words(text, "state_dir()").map(|(at, m)| (at + m.len(), format!("plugins/{p}"))));
        }
        for (end, prefix) in starts {
            let resolve = |name: &str| resolve(&consts, file, name);
            follow(file, text, end, &prefix, &resolve, &mut out);
            // `let x = <base>;` and then `x.join(..)` in the rest of that function
            let ends_let = text[end..].trim_start_matches('?').starts_with(';') || text[end..].trim_start().starts_with("else");
            if let Some(var) = bound_to(&text[..end]).filter(|_| ends_let) {
                let after = &text[end..];
                let after = &after[..after.find("fn ").unwrap_or(after.len())];
                for (at, m) in after.match_indices(&format!("{var}.join(")) {
                    let before = after[..at].chars().last();
                    if before.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.') {
                        follow(file, after, at + m.len() - ".join(".len(), &prefix, &resolve, &mut out);
                    }
                }
            }
        }
    }
    out
}

/// The places `name` starts a word in `text`: not inside a longer name.
fn words<'a>(text: &'a str, name: &'a str) -> impl Iterator<Item = (usize, &'a str)> + 'a {
    text.match_indices(name).filter(move |(at, _)| text[..*at].chars().last().is_none_or(|c| !c.is_alphanumeric() && c != '_'))
}

fn resolve(consts: &HashMap<String, Vec<(String, String)>>, file: &str, name: &str) -> Option<String> {
    let name = name.rsplit("::").next()?;
    let all = consts.get(name)?;
    all.iter().find(|(f, _)| f == file).or(all.first()).map(|(_, v)| v.clone())
}

/// The variable a `let` binds the expression ending at the end of
/// `before` to, when the statement starts there.
fn bound_to(before: &str) -> Option<&str> {
    let stmt = &before[before.rfind([';', '{', '}']).map_or(0, |i| i + 1)..];
    let rest = stmt.trim_start().strip_prefix("let ")?;
    let rest = rest.strip_prefix("mut ").unwrap_or(rest);
    let rest = rest.strip_prefix("Ok(").unwrap_or(rest);
    let var: &str = &rest[..rest.find(|c: char| !c.is_alphanumeric() && c != '_')?];
    (!var.is_empty()).then_some(var)
}

/// The `.join(..)` chain at `at` of `text`, from `prefix`.
fn follow(file: &str, text: &str, mut at: usize, prefix: &str, resolve: &dyn Fn(&str) -> Option<String>, out: &mut Vec<Found>) {
    let mut path = prefix.to_string();
    let mut grew = false;
    loop {
        let rest = &text[at..];
        let rest_q = rest.strip_prefix('?').unwrap_or(rest);
        at += rest.len() - rest_q.len();
        let Some(arg) = rest_q.strip_prefix(".join(") else { break };
        let (part, used) = if let Some(lit) = arg.strip_prefix('"') {
            let Some(body) = literal_body(lit) else { break };
            if !lit[body.len() + 1..].starts_with(')') {
                break;
            }
            (body.to_string(), body.len() + 2)
        } else {
            let Some(close) = arg.find(')') else { break };
            let ident = &arg[..close];
            if !ident.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b':' || b.is_ascii_lowercase()) {
                break;
            }
            let last = ident.rsplit("::").next().unwrap_or(ident);
            if !last.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') || last.is_empty() {
                break;
            }
            match resolve(ident) {
                Some(v) => (v, close),
                None => break,
            }
        };
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&part);
        grew = true;
        at += ".join(".len() + used + 1;
    }
    if grew {
        out.push(Found { file: file.to_string(), path });
    }
}

#[test]
fn state_paths_in_source_are_registered() {
    let srcs = sources(Path::new(env!("CARGO_MANIFEST_DIR")));
    assert!(srcs.len() > 300, "read {} sources, so this test is looking in the wrong place", srcs.len());
    let found = paths(&srcs);
    assert!(found.len() > 50, "found {} paths, so the scan no longer reads the source's shapes", found.len());
    for f in &found {
        println!("{}\t{}", f.path, f.file);
    }
    let missing: Vec<String> =
        found.iter().filter(|f| !super::is_registered(&f.path)).map(|f| format!("{} (built in {})", f.path, f.file)).collect();
    assert!(missing.is_empty(), "paths under the state root that host::state_map does not know:\n  {}", missing.join("\n  "));
    println!("{} paths built on the state root, all registered", found.len());
}

#[test]
fn the_scan_reads_the_shapes_paths_are_built_in() {
    let src = |file: &str, text: &str| (file.to_string(), flat(production(text)));
    let srcs = [
        src("x/a.rs", "const F: &str = \"f.tsv\";\nfn p() { state_root().join(\"zombie\")\n    .join(F); }\nfn q() { default_state_root().join(\"no\"); }"),
        src(
            "x/plugins/kimi/b.rs",
            "fn l(h: &dyn H) { let Ok(dir) = h.state_dir() else { return };\n let t = dir.join(REFUSED); }\nconst REFUSED: &str = \"refused.tsv\";",
        ),
        src("x/c.rs", "fn r() { let root = paths::state_root();\n root.join(\"a\").join(name).join(\"b\"); session_dir(7).join(\"guard\"); }"),
        src("x/d.rs", "fn s() { state_root().join(\"live\"); }\n#[cfg(test)]\nmod tests { fn t() { state_root().join(\"test-only\"); } }"),
    ];
    let mut got: Vec<String> = paths(&srcs).into_iter().map(|f| f.path).collect();
    got.sort();
    assert_eq!(got, ["a", "live", "plugins/kimi/refused.tsv", "sessions/1/guard", "zombie/f.tsv"]);
}
