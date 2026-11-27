use std::process::Command;

#[path = "crates/marspot-term/build_sha.rs"]
mod build_sha;

fn main() {
    let head = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let env = std::env::var("MARSPOT_GIT_SHA").ok();
    let tag = build_sha::choose(head.as_deref(), dirty, env.as_deref());
    println!("cargo:rustc-env=MARSPOT_GIT_SHA={}", tag);
    println!("cargo:rerun-if-env-changed=MARSPOT_GIT_SHA");
    println!("cargo:rerun-if-changed=crates/marspot-term/build_sha.rs");

    // Build timestamp — easy "is this really the latest binary?" check
    // when iterating fast. Emit as ISO-8601 in the local timezone.
    let build_ts = std::process::Command::new("date")
        .args(["+%Y-%m-%dT%H:%M:%S"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MARSPOT_BUILD_TS={}", build_ts);

    // Per-layer version vector. Each binary embeds its own layer's
    // version via `env!("MARSPOT_VERSION_<LAYER>")`; the title bar shows
    // the L2 (core) version as THE marspot version.
    let vv = std::fs::read_to_string("version-vector.toml").unwrap_or_default();
    for layer in ["shell", "core", "session"] {
        let version = vv
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == layer)
            // Strip any trailing inline comment (`shell = "0.2.0"  # L1`)
            // before unquoting + trimming.
            .map(|(_, v)| {
                v.split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "cargo:rustc-env=MARSPOT_VERSION_{}={}",
            layer.to_uppercase(),
            version
        );
    }
    println!("cargo:rerun-if-changed=version-vector.toml");

    println!("cargo:rerun-if-changed=build.rs");
    build_sha::print_git_watch();
}
