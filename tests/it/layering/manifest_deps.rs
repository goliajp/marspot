//! A reader for the dependency tables of a Cargo manifest, enough for
//! the layering rules to follow path dependencies without cargo.

use std::path::{Path, PathBuf};

/// One dependency entry of a manifest, as written.
#[derive(Debug, Default, PartialEq)]
pub struct Dep {
    pub name: String,
    pub path: Option<String>,
    pub workspace: bool,
}

/// What a section header holds: `Some(None)` a list of dependencies,
/// `Some(Some(name))` the table of one, `None` anything else.
pub type Section = Option<Option<String>>;

/// The dependency sections that are linked into the binaries (dev
/// dependencies are not).
pub fn linked_section(header: &str) -> Section {
    let rest = match header.strip_prefix("target.") {
        // the cfg part may be quoted and hold dots
        Some(t) => match t.chars().next() {
            Some(q @ ('\'' | '"')) => t[1..].split_once(q)?.1.strip_prefix('.')?,
            _ => t.split_once('.')?.1,
        },
        None => header,
    };
    for kind in ["dependencies", "build-dependencies"] {
        if rest == kind {
            return Some(None);
        }
        if let Some(name) = rest.strip_prefix(kind).and_then(|r| r.strip_prefix('.')) {
            return Some(Some(name.to_string()));
        }
    }
    None
}

pub fn workspace_section(header: &str) -> Section {
    match header.strip_prefix("workspace.dependencies")? {
        "" => Some(None),
        r => r.strip_prefix('.').map(|n| Some(n.to_string())),
    }
}

/// `s` up to a `#` outside a string.
fn strip_comment(s: &str) -> &str {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '#') => return &s[..i],
            _ => {}
        }
    }
    s
}

/// The `key = value` pairs of an inline table's inside, split at the
/// commas that are outside strings and arrays.
fn inline_pairs(s: &str) -> Vec<(&str, &str)> {
    let (mut depth, mut quote, mut start, mut parts) = (0, None, 0, Vec::new());
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter().filter_map(|p| p.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).collect()
}

fn unquote(v: &str) -> Option<String> {
    let q = v.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    v[1..].strip_suffix(q).map(str::to_string)
}

/// Sets one key of a dependency: `path`, `workspace` and `package` count.
fn apply(dep: &mut Dep, key: &str, value: &str) {
    match key {
        "path" => dep.path = unquote(value),
        "workspace" => dep.workspace = value == "true",
        "package" => dep.name = unquote(value).unwrap_or_default(),
        _ => {}
    }
}

/// Every dependency in the sections `section` accepts, in the forms
/// `name = "1"`, `name = { .. }`, `name.key = ..` and `[<section>.name]`.
pub fn manifest_deps(text: &str, section: fn(&str) -> Section) -> Vec<Dep> {
    let mut current: Section = None;
    let mut out: Vec<Dep> = Vec::new();
    // the key a run of `name.key = ..` lines was written under, before any rename
    let mut dotted: Option<String> = None;
    for line in text.lines().map(|l| strip_comment(l).trim()) {
        if let Some(h) = line.strip_prefix('[') {
            current = section(h.trim_end_matches(']').trim());
            dotted = None;
            if let Some(Some(name)) = &current {
                out.push(Dep { name: name.clone(), ..Dep::default() });
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match &current {
            None => {}
            Some(Some(_)) => apply(out.last_mut().expect("the table's entry"), key, value),
            Some(None) => {
                let (name, sub) = key.split_once('.').map_or((key, None), |(n, k)| (n, Some(k)));
                if sub.is_none() || dotted.as_deref() != Some(name) {
                    out.push(Dep { name: name.to_string(), ..Dep::default() });
                }
                dotted = sub.map(|_| name.to_string());
                let dep = out.last_mut().expect("just pushed");
                match (sub, value.strip_prefix('{').and_then(|v| v.strip_suffix('}'))) {
                    (Some(k), _) => apply(dep, k, value),
                    (None, Some(inner)) => inline_pairs(inner).into_iter().for_each(|(k, v)| apply(dep, k, v)),
                    (None, None) => {}
                }
            }
        }
    }
    out
}

/// The deps that resolve to a path: their own, relative to `base`, or
/// the workspace entry's, relative to `ws_base`.
pub fn resolve(deps: Vec<Dep>, base: &Path, ws: &[Dep], ws_base: &Path) -> Vec<(String, PathBuf)> {
    deps.into_iter()
        .filter_map(|d| {
            let path = match (&d.path, d.workspace) {
                (Some(p), _) => base.join(p),
                (None, true) => ws_base.join(ws.iter().find(|w| w.name == d.name)?.path.as_ref()?),
                (None, false) => return None,
            };
            Some((d.name, path))
        })
        .collect()
}

#[test]
fn the_manifest_reader_takes_only_path_keys() {
    let text = r#"
[dependencies]
pathdiff = "1"
pathfinder = { version = "1", features = ["a, path = 'x'", "b"] } # path = "../no"
# gone = { path = "../gone" }
inline = { path = "../inline", default-features = false }
dotted.version = "1"
dotted.path = "../dotted"
viaws.workspace = true
viaws2 = { workspace = true }
renamed = { package = "real", path = "../real" }

[dependencies.table]
version = "2"
path = "../table"

[target.'cfg(target_os = "macos")'.dependencies]
mac = { path = "../mac" }

[dev-dependencies]
devonly = { path = "../dev" }

[lints]
workspace = true
"#;
    let ws_text = "[workspace.dependencies]\nviaws = { path = \"crates/viaws\" }\nviaws2.path = \"crates/viaws2\"\n";
    let ws = manifest_deps(ws_text, workspace_section);
    let got = resolve(manifest_deps(text, linked_section), Path::new("/c"), &ws, Path::new("/w"));
    let want: Vec<(String, PathBuf)> = [
        ("inline", "/c/../inline"),
        ("dotted", "/c/../dotted"),
        ("viaws", "/w/crates/viaws"),
        ("viaws2", "/w/crates/viaws2"),
        ("real", "/c/../real"),
        ("table", "/c/../table"),
        ("mac", "/c/../mac"),
    ]
    .into_iter()
    .map(|(n, p)| (n.to_string(), PathBuf::from(p)))
    .collect();
    assert_eq!(got, want);
}
