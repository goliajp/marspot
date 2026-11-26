//! A second install of the commit that is already running swaps
//! nothing, `marspot-peer` included, and only when the source tree
//! confirms the commit the new binaries name.
//!
//! The decision is install-local's own (`bin/_install-layers.sh
//! explain`), run between two directories of the real binaries: the
//! "running" set and a second build of the same commit.  The second
//! build is the first one signed again, which is what changes the bytes
//! of one build from one install to the next: install-local signs every
//! time, and the signature records when.  peer once skipped the
//! same-commit step the other three take and was replaced on every
//! install for that reason alone.
//!
//! Each copy's fingerprint is set to the commit of a small git
//! repository made for the test, which stands for the tree the binaries
//! were built from.  A build script that did not rerun leaves a clean
//! commit in a binary built from a dirty tree, or from a later commit,
//! so a fingerprint is taken as true only while that tree is clean and
//! at the commit it names; otherwise the bytes decide.

use std::path::{Path, PathBuf};
use std::process::Command;

use marspot_term::test_env::{TempDir, built_binary};

const BINS: [&str; 4] = ["marspot-shell", "marspot-core", "marspot-session", "marspot-peer"];

fn built(name: &str) -> PathBuf {
    match name {
        "marspot-shell" => env!("CARGO_BIN_EXE_marspot-shell").into(),
        "marspot-core" => env!("CARGO_BIN_EXE_marspot-core").into(),
        other => built_binary(other),
    }
}

/// Rewrite every well-formed `MARSPOT_FP=<sha>|<ts>|END` in `bytes` to
/// name `sha`, padded with NULs to the original length so no other byte
/// moves.  Returns how many it rewrote.
fn set_fingerprint(bytes: &mut [u8], sha: &str) -> usize {
    const NEEDLE: &[u8] = b"MARSPOT_FP=";
    let mut at = 0;
    let mut n = 0;
    while let Some(off) = bytes[at..].windows(NEEDLE.len()).position(|w| w == NEEDLE) {
        let start = at + off;
        let body = start + NEEDLE.len();
        at = body;
        let tail = &bytes[body..bytes.len().min(body + 96)];
        let Some(end) = tail.windows(4).position(|w| w == b"|END") else { continue };
        let marker = &tail[..end];
        let fields: Vec<&[u8]> = marker.split(|&b| b == b'|').collect();
        let printable = marker.iter().all(|b| b.is_ascii_graphic());
        if fields.len() != 2 || fields.iter().any(|f| f.is_empty()) || !printable {
            continue;
        }
        let new = format!("MARSPOT_FP={sha}|1|END");
        let old_len = NEEDLE.len() + end + 4;
        assert!(new.len() <= old_len, "{new} does not fit over the marker at {start}");
        bytes[start..start + new.len()].copy_from_slice(new.as_bytes());
        bytes[start + new.len()..start + old_len].fill(0);
        at = start + old_len;
        n += 1;
    }
    n
}

fn copy_with_commit(name: &str, dir: &Path, sha: &str) -> PathBuf {
    let mut bytes = std::fs::read(built(name)).unwrap();
    assert!(set_fingerprint(&mut bytes, sha) > 0, "{name} has no MARSPOT_FP marker");
    let to = dir.join(name);
    std::fs::write(&to, bytes).unwrap();
    // peer_decision takes only an executable as built
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&to, mode).unwrap();
    to
}

/// Sign `bin` again, as an install does.
fn resign(bin: &Path) {
    let out = Command::new("codesign").args(["--force", "--sign", "-"]).arg(bin).output().unwrap();
    assert!(out.status.success(), "codesign: {}", String::from_utf8_lossy(&out.stderr));
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit `src/lib.rs` with `text` in `repo`; returns the commit as a
/// build names it.
fn commit(repo: &Path, text: &str) -> String {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), text).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", text]);
    git(repo, &["rev-parse", "--short=8", "HEAD"])
}

/// The `INSTALL_LAYERS` line an install from `running` to `target`,
/// built from `src`, writes.
fn explain(running: &Path, target: &Path, src: &Path) -> String {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/_install-layers.sh");
    let out = Command::new("bash").arg(script).arg("explain").arg(running).arg(target).arg(src).output().unwrap();
    assert!(out.status.success(), "explain: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let key = format!("{key}=");
    line.split(' ').find_map(|f| f.strip_prefix(key.as_str())).unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// A source tree clean at one commit, the running set and a second
/// build of that commit.
struct Case {
    _tmp: TempDir,
    src: PathBuf,
    commit: String,
    running: PathBuf,
    target: PathBuf,
}

impl Case {
    fn new() -> Self {
        let tmp = TempDir::new("install-layers");
        let (src, running, target) = (tmp.join("src"), tmp.join("running"), tmp.join("target"));
        for d in [&src, &running, &target] {
            std::fs::create_dir_all(d).unwrap();
        }
        git(&src, &["init", "-q"]);
        let commit = commit(&src, "first");
        for name in BINS {
            let old = copy_with_commit(name, &running, &commit);
            let new = copy_with_commit(name, &target, &commit);
            resign(&new);
            let differ = std::fs::read(&old).unwrap() != std::fs::read(&new).unwrap();
            assert!(differ, "signing {name} again left its bytes as they were; the case is not built");
        }
        Case { _tmp: tmp, src, commit, running, target }
    }

    fn explain(&self) -> String {
        explain(&self.running, &self.target, &self.src)
    }
}

const ALL_BYTES_DIFFER: &str = "shell:bytes-differ,core:bytes-differ,session:bytes-differ,peer:bytes-differ";

#[test]
fn a_second_install_of_the_running_commit_swaps_nothing() {
    let case = Case::new();
    let line = case.explain();
    assert_eq!(field(&line, "swap"), "none", "{line}");
    assert_eq!(
        field(&line, "why"),
        "shell:same-commit,core:same-commit,session:same-commit,peer:same-commit",
        "{line}"
    );
}

/// An unstaged edit in the tree: the binaries may hold it under the
/// clean name, so every layer is compared by its bytes.
#[test]
fn a_dirty_source_tree_compares_bytes() {
    let case = Case::new();
    std::fs::write(case.src.join("src/lib.rs"), "edited").unwrap();
    let line = case.explain();
    assert_eq!(field(&line, "why"), ALL_BYTES_DIFFER, "{line}");
    assert_eq!(field(&line, "swap"), "shell,core,session,peer", "{line}");
}

/// The tree moved on to a later commit the binaries do not name.
#[test]
fn a_source_tree_at_another_commit_compares_bytes() {
    let case = Case::new();
    let later = commit(&case.src, "second");
    assert_ne!(later, case.commit);
    let line = case.explain();
    assert_eq!(field(&line, "why"), ALL_BYTES_DIFFER, "{line}");
}

#[test]
fn a_peer_from_another_commit_is_replaced() {
    let case = Case::new();
    copy_with_commit("marspot-peer", &case.target, "0ddba11a");
    let line = case.explain();
    assert_eq!(field(&line, "swap"), "peer", "{line}");
    assert!(field(&line, "why").ends_with(",peer:bytes-differ"), "{line}");
}

/// A `-dirty` build names no commit: only identical bytes are kept.
#[test]
fn a_dirty_peer_is_compared_by_its_bytes() {
    let case = Case::new();
    let dirty = format!("{}-dirty", case.commit);
    let old = copy_with_commit("marspot-peer", &case.running, &dirty);
    let new = copy_with_commit("marspot-peer", &case.target, &dirty);
    assert!(field(&case.explain(), "why").ends_with(",peer:bytes-same"));
    resign(&new);
    assert_ne!(std::fs::read(&old).unwrap(), std::fs::read(&new).unwrap());
    let line = case.explain();
    assert!(field(&line, "why").ends_with(",peer:bytes-differ"), "{line}");
    assert!(field(&line, "swap").ends_with("peer"), "{line}");
}
