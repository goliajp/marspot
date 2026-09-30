use std::process::Command;

fn main() {
    let sha = Command::new("git")
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
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let tag = if dirty {
        format!("{}-dirty", sha)
    } else {
        sha
    };

    println!("cargo:rustc-env=MARSPOT_GIT_SHA={}", tag);

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

    // Re-run when the commit changes.  `.git/HEAD` only changes on a
    // branch *switch* — a commit on the current branch leaves HEAD
    // (`ref: refs/heads/<branch>`) byte-identical and just moves the ref
    // it points at, so watching HEAD alone bakes a stale sha after every
    // commit.  Resolve HEAD's ref and watch that loose-ref file too, plus
    // packed-refs (post-`git gc`) and the index (staging).
    println!("cargo:rerun-if-changed=build.rs");
    // Only watch paths that EXIST.  A `rerun-if-changed` naming a
    // missing file is forever stale to cargo, so the script reran on every
    // single cargo invocation, re-emitted a fresh `MARSPOT_BUILD_TS`
    // (`date`), and with it recompiled and relinked every crate and
    // test harness — 3.6 s wall / 19.7 s CPU, 47 rustc runs, for a tree
    // with no change in it (2026-09-28).  `packed-refs` does not exist
    // until `git gc` writes one; the loose ref does, and after a gc the
    // two swap places, which this handles by asking each time.
    for p in [".git/HEAD", ".git/packed-refs", ".git/index"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(refname) = head.strip_prefix("ref: ").map(str::trim) {
            let p = format!(".git/{refname}");
            if std::path::Path::new(&p).exists() {
                println!("cargo:rerun-if-changed={p}");
            }
        }
}
