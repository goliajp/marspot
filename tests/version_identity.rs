//! The four numbers that were all called "the version" must agree.
//!
//! They did not, and the cost was a silent one: the shell handed the
//! updater a layer's build number, the updater compared it to a
//! release tag, and no tag was ever going to be newer than `0.12.x`.
//! The update channel was dead from June and nothing said so.
//!
//! These tests are what would have said so.

use std::process::Command;

fn cargo_toml_version() -> String {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    // The first `version = "…"` after `[package]`, which is the
    // product's own — not a dependency's.
    let package = text
        .split("[package]")
        .nth(1)
        .expect("Cargo.toml has a [package] section");
    for line in package.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version")
            && let Some(v) = rest.split('"').nth(1) {
                return v.to_string();
            }
        if line.starts_with('[') {
            break;
        }
    }
    panic!("no version in [package]");
}

#[test]
fn the_product_version_comes_from_cargo_toml() {
    assert_eq!(
        marspot::PRODUCT_VERSION,
        cargo_toml_version(),
        "the constant and the manifest disagree, which is how four \
         different numbers all came to be called the version"
    );
}

#[test]
fn the_reader_can_fail() {
    // The check above is only worth something if this parse would
    // notice a different number.  Prove it reads a value rather than
    // returning whatever it was compared against.
    let v = cargo_toml_version();
    assert!(
        v.split('.').count() >= 2 && v.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "parsed {v:?} out of Cargo.toml, which is not a version"
    );
    assert_ne!(v, "", "an empty read would equal nothing and pass everything");
}

#[test]
fn a_layer_build_number_is_not_the_product_version() {
    // The specific confusion that broke the channel.  These two are
    // allowed to be anything, but if they are ever made equal it will
    // be because someone conflated them again.
    assert_ne!(
        marspot::PRODUCT_VERSION,
        marspot::build_ids::CORE,
        "the product version and the core layer's build number are \
         different kinds of number and should not be kept in lockstep"
    );
}

#[test]
fn the_newest_tag_matches_the_product_version() {
    let out = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "tag", "--sort=-v:refname"])
        .output();
    let Ok(out) = out else {
        eprintln!("SKIPPED: git not runnable here");
        return;
    };
    let tags = String::from_utf8_lossy(&out.stdout);
    let Some(newest) = tags.lines().next() else {
        // Saying so matters: "no tags" and "the tag matches" are the
        // same silent pass otherwise, and this repository's history
        // was rebuilt once, which dropped every tag.
        eprintln!("SKIPPED: no tags in this repository yet");
        return;
    };
    assert_eq!(
        newest.trim_start_matches('v'),
        marspot::PRODUCT_VERSION,
        "newest tag {newest} does not name the product version; a release \
         cut from it would be refused by the workflow's tag check"
    );
}
