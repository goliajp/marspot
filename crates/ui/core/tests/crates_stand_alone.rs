//! The interface crates depend on nothing outside their own directory.
//!
//! Cargo already refuses a dependency that is not declared, so the
//! thing to guard is the declaration: this reads every manifest beside
//! this crate's and fails on a dependency that is anything other than
//! a path to a sibling.  The application these crates came out of is
//! not a sibling, and neither is anything on a registry.

use std::fs;
use std::path::Path;

fn is_dependency_table(section: &str) -> bool {
    let last = section.rsplit('.').next().unwrap_or(section);
    matches!(last, "dependencies" | "dev-dependencies" | "build-dependencies")
}

/// `name` when `section` is one dependency written as its own table:
/// `[dependencies.name]`.
fn dependency_of_table(section: &str) -> Option<&str> {
    let (table, name) = section.rsplit_once('.')?;
    is_dependency_table(table).then_some(name)
}

fn is_sibling_path(spec: &str) -> bool {
    let Some(at) = spec.find("path") else { return false };
    let Some(open) = spec[at..].find('"') else { return false };
    let rest = &spec[at + open + 1..];
    let Some(close) = rest.find('"') else { return false };
    let path = &rest[..close];
    path.strip_prefix("../").is_some_and(|name| !name.is_empty() && !name.contains(['/', '.']))
}

/// Every dependency a manifest declares that is not a sibling crate.
fn outside_dependencies(manifest: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut section = String::new();
    // a dependency written as its own table is judged when the table ends
    let mut table: Option<(String, bool)> = None;
    fn close(table: &mut Option<(String, bool)>, found: &mut Vec<String>) {
        if let Some((name, sibling)) = table.take()
            && !sibling
        {
            found.push(name);
        }
    }
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            close(&mut table, &mut found);
            section = header.trim_end_matches(']').trim().to_string();
            if let Some(name) = dependency_of_table(&section) {
                table = Some((name.to_string(), false));
            }
            continue;
        }
        if let Some((_, sibling)) = table.as_mut() {
            *sibling |= line.starts_with("path") && is_sibling_path(line);
        } else if is_dependency_table(&section)
            && let Some((name, spec)) = line.split_once('=')
            && !is_sibling_path(spec)
        {
            found.push(name.trim().to_string());
        }
    }
    close(&mut table, &mut found);
    found
}

fn manifests() -> Vec<std::path::PathBuf> {
    let family = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("the crate has a parent");
    let mut out: Vec<_> = fs::read_dir(family)
        .expect("read the directory the interface crates live in")
        .map(|e| e.expect("dir entry").path().join("Cargo.toml"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

#[test]
fn no_interface_crate_depends_on_anything_outside_its_family() {
    let mut offenders = Vec::new();
    for path in manifests() {
        let text = fs::read_to_string(&path).expect("read a manifest");
        for dep in outside_dependencies(&text) {
            offenders.push(format!("{}: {dep}", path.display()));
        }
    }
    assert!(offenders.is_empty(), "dependencies that reach outside: {offenders:?}");
}

#[test]
fn the_scan_reads_this_crates_own_manifest() {
    let own = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert!(manifests().contains(&own), "the walk found {:?}", manifests());
}

#[test]
fn the_check_can_fail() {
    let app = r#"
[package]
name = "x"

[dependencies]
marspot-term = { path = "../../marspot-term" }
"#;
    assert_eq!(outside_dependencies(app), ["marspot-term"]);

    let renamed = "[dependencies]\nterm = { package = \"marspot-term\", path = \"../../marspot-term\" }\n";
    assert_eq!(outside_dependencies(renamed), ["term"]);

    let as_table = "[dev-dependencies.marspot]\npath = \"../../..\"\n";
    assert_eq!(outside_dependencies(as_table), ["marspot"]);

    let for_a_target = "[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n";
    assert_eq!(outside_dependencies(for_a_target), ["libc"]);

    let inherited = "[dependencies]\nserde = { workspace = true }\n";
    assert_eq!(outside_dependencies(inherited), ["serde"]);
}

#[test]
fn a_sibling_is_allowed_and_other_tables_are_not_mistaken_for_dependencies() {
    let fine = r#"
[package]
name = "widgets"
version = "0.1.0"

[dependencies]
golia-ui-core = { path = "../core" }   # the sibling

[dev-dependencies.golia-ui-core]
path = "../core"

[lints]
workspace = true
"#;
    assert!(outside_dependencies(fine).is_empty(), "{:?}", outside_dependencies(fine));
}
