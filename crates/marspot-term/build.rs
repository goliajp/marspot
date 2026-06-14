// Mirror of the root crate's build.rs — emits MARSPOT_GIT_SHA and
// MARSPOT_BUILD_TS so `crates/marspot-session` (which links
// marspot-term but NOT the root marspot crate) can embed the same
// fingerprint into its binary's rodata. install-local.sh then has a
// reliable way to compare a fresh build against the running session
// binary and skip-stage when both are at the same clean commit.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let tag = if dirty { format!("{}-dirty", sha) } else { sha };
    println!("cargo:rustc-env=MARSPOT_GIT_SHA={}", tag);

    let build_ts = Command::new("date")
        .args(["+%Y-%m-%dT%H:%M:%S"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MARSPOT_BUILD_TS={}", build_ts);

    // Per-layer version vector (read from workspace root). Each binary
    // that links marspot-term embeds its own layer's version.
    let vv = std::fs::read_to_string("../../version-vector.toml").unwrap_or_default();
    for layer in ["shell", "core", "session", "shelld"] {
        let version = vv
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == layer)
            .map(|(_, v)| {
                v.split('#').next().unwrap_or("").trim().trim_matches('"').to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "cargo:rustc-env=MARSPOT_VERSION_{}={}",
            layer.to_uppercase(),
            version
        );
    }
    println!("cargo:rerun-if-changed=../../version-vector.toml");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    println!("cargo:rerun-if-changed=../../.git/index");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(refname) = head.strip_prefix("ref: ").map(str::trim) {
            println!("cargo:rerun-if-changed=../../.git/{refname}");
        }
    }
}
