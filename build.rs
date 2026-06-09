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

    // Always re-run so build time stays fresh.
    println!("cargo:rerun-if-changed=build.rs");
    // Re-run when HEAD moves (branch switch, checkout) or staged changes change.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
