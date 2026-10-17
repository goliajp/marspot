//! The `test-update-key` feature makes the updater trust a second
//! signing key.  It must never reach a build that ships: these check
//! every path that builds one, and every manifest that could turn the
//! feature on behind their backs.

use std::path::{Path, PathBuf};

const FEATURE: &str = "test-update-key";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The scripts and workflow that produce what users run.
const SHIPPING: &[&str] = &[
    "bin/install-local.sh",
    "bin/build-release-tarball.sh",
    "bin/build-app-bundle.sh",
    ".github/workflows/release.yml",
];

/// A release build in any of them names no feature at all, so it
/// cannot name this one, and none of them mentions it anywhere.
#[test]
fn no_shipping_build_turns_the_feature_on() {
    let mut release_builds = 0;
    for rel in SHIPPING {
        let body = read(rel);
        assert!(!body.contains(FEATURE), "{rel} mentions {FEATURE}");
        // a command continued over several lines is one command
        let body = body.replace("\\\n", " ");
        for (n, line) in body.lines().enumerate() {
            if line.contains("cargo build") && line.contains("--release") {
                release_builds += 1;
                assert!(
                    !line.contains("--features") && !line.contains("--all-features") && !line.contains(" -F"),
                    "{rel}:{}: a release build with features: {line}",
                    n + 1
                );
            }
        }
    }
    // the checks above are vacuous if the release builds moved
    assert!(release_builds >= 3, "found only {release_builds} release builds to check");
}

/// No manifest turns the feature on by default or through a
/// dependency's feature list: the root's pass-through definition is
/// the one place it may appear.
#[test]
fn no_manifest_turns_the_feature_on() {
    let mut manifests = vec![PathBuf::from("Cargo.toml")];
    for dir in std::fs::read_dir(repo().join("crates")).unwrap().flatten() {
        if dir.path().join("Cargo.toml").exists() {
            manifests.push(Path::new("crates").join(dir.file_name()).join("Cargo.toml"));
        }
    }
    assert!(manifests.len() >= 3, "found the workspace's manifests");
    for m in &manifests {
        let body = read(&m.to_string_lossy());
        for (n, line) in body.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("");
            if !line.contains(FEATURE) {
                continue;
            }
            let definition = line.trim_start().starts_with(FEATURE);
            assert!(
                definition,
                "{}:{}: turns {FEATURE} on: {line}",
                m.display(),
                n + 1
            );
        }
        // a definition must not be pulled in by `default`
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("");
            if line.trim_start().starts_with("default") {
                assert!(!line.contains(FEATURE), "{}: default includes {FEATURE}", m.display());
            }
        }
    }
}
