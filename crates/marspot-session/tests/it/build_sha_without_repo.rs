//! A session built where there is no repository names the commit the
//! syncing side passed in `MARSPOT_GIT_SHA`, in both places a build is
//! read: the `MARSPOT_FP` marker install-local compares, which comes
//! from the engine's build script, and the `git=` of the start-up log,
//! which comes from the session's own.  The two scripts once disagreed:
//! a runner build carried the right fingerprint and logged `git=unknown`.
//!
//! The copy is a real one: the three crates the session links, built by
//! cargo out of a directory git cannot find a repository from.  It stays
//! under the target directory between runs so cargo rebuilds only what
//! changed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_term::test_env::sandbox_state_dir;

use crate::support::{COLS, ROWS, Session};

// a full sha with -dirty, which the build scripts shorten to 8
const PASSED: &str = "0123456789abcdef0123456789abcdef01234567-dirty";
const NAMED: &str = "01234567-dirty";

const CRATES: [&str; 3] = ["marspot-term", "marspot-linkify", "marspot-session"];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// `target/`, the parent of the profile directory the session binary is in
fn target_dir() -> PathBuf {
    let bin = Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    bin.parent().and_then(Path::parent).expect("target/<profile>/marspot-session").to_path_buf()
}

/// Write `bytes` to `to` only when they differ, so an unchanged file
/// keeps the mtime cargo judges freshness by.
fn put(to: &Path, bytes: &[u8]) {
    if std::fs::read(to).is_ok_and(|have| have == bytes) {
        return;
    }
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::write(to, bytes).unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            put(&to.join(entry.file_name()), &std::fs::read(&path).unwrap());
        }
    }
}

/// The session's crates as a workspace of their own, with what their
/// sources read from the workspace root.
fn copy_tree(tree: &Path) {
    let root = workspace();
    for name in CRATES {
        let (from, to) = (root.join("crates").join(name), tree.join("crates").join(name));
        put(&to.join("Cargo.toml"), &std::fs::read(from.join("Cargo.toml")).unwrap());
        copy_dir(&from.join("src"), &to.join("src"));
        for file in ["build.rs", "build_sha.rs"] {
            if let Ok(bytes) = std::fs::read(from.join(file)) {
                put(&to.join(file), &bytes);
            }
        }
    }
    for file in ["Cargo.lock", "version-vector.toml", "keys/marspot-update.pub"] {
        put(&tree.join(file), &std::fs::read(root.join(file)).unwrap());
    }
    let members = CRATES.map(|c| format!("\"crates/{c}\"")).join(", ");
    let manifest = format!(
        "[workspace]\nmembers = [{members}]\nresolver = \"3\"\n\n\
         [workspace.lints.clippy]\n\n[profile.dev]\ndebug = 1\n"
    );
    put(&tree.join("Cargo.toml"), manifest.as_bytes());
}

/// A command run in `tree` that cannot see the repository above it:
/// the target directory lives inside the checkout.
fn outside_git(program: impl AsRef<std::ffi::OsStr>, tree: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(tree)
        .env("GIT_CEILING_DIRECTORIES", tree.parent().unwrap())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    cmd
}

fn build(base: &Path) -> PathBuf {
    let tree = base.join("tree");
    copy_tree(&tree);
    let probe = outside_git("git", &tree).args(["rev-parse", "HEAD"]).output().unwrap();
    assert!(!probe.status.success(), "git found a repository from {}", tree.display());

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = base.join("target");
    let out = outside_git(cargo, &tree)
        .args(["build", "--offline", "-p", "marspot-session", "--bin", "marspot-session"])
        .env("CARGO_TARGET_DIR", &target)
        .env("MARSPOT_GIT_SHA", PASSED)
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo build failed:\n{}", String::from_utf8_lossy(&out.stderr));
    target.join("debug/marspot-session")
}

/// The `SESSION_BOOT` line of the session's log, once it is written.
fn boot_line(logs: &Path) -> Option<String> {
    for entry in std::fs::read_dir(logs).ok()? {
        let text = std::fs::read_to_string(entry.ok()?.path()).unwrap_or_default();
        if let Some(line) = text.lines().find(|l| l.contains("SESSION_BOOT")) {
            return Some(line.to_string());
        }
    }
    None
}

#[test]
fn a_session_built_without_a_repository_names_the_passed_commit() {
    let bin = build(&target_dir().join("build-sha-copy"));

    let build = marspot_term::build::of_file(&bin).expect("the binary carries MARSPOT_FP");
    assert!(build.starts_with(&format!("MARSPOT_FP={NAMED}|")), "fingerprint {build:?}");

    let mut sandbox = sandbox_state_dir();
    sandbox.env.remove("MARSPOT_LOG_DIR");
    let logs = sandbox.path().join("logs");
    let mut session = Session::spawn(&bin, COLS, ROWS);
    let deadline = Instant::now() + Duration::from_secs(10);
    let line = loop {
        if let Some(line) = boot_line(&logs) {
            break line;
        }
        assert!(Instant::now() < deadline, "no SESSION_BOOT in {} within 10 s", logs.display());
        std::thread::sleep(Duration::from_millis(50));
    };
    session.kill();
    assert!(
        line.split(['\t', ' ']).any(|field| field == format!("git={NAMED}")),
        "start-up log: {line}"
    );
}
