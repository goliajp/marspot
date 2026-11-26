//! A second install of the commit that is already running swaps
//! nothing, `marspot-peer` included.
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
//! Each copy's fingerprint is set to a fixed clean commit, so the
//! answer does not depend on whether the tree the suite was built from
//! had uncommitted changes.

use std::path::{Path, PathBuf};
use std::process::Command;

use marspot_term::test_env::{TempDir, built_binary};

const COMMIT: &str = "abcdef12";
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

/// The `INSTALL_LAYERS` line an install from `running` to `target` writes.
fn explain(running: &Path, target: &Path) -> String {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/_install-layers.sh");
    let out = Command::new("bash").arg(script).arg("explain").arg(running).arg(target).output().unwrap();
    assert!(out.status.success(), "explain: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let key = format!("{key}=");
    line.split(' ').find_map(|f| f.strip_prefix(key.as_str())).unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// The running set and a second build of the same commit.
fn two_builds(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let (running, target) = (tmp.join("running"), tmp.join("target"));
    std::fs::create_dir_all(&running).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    for name in BINS {
        let old = copy_with_commit(name, &running, COMMIT);
        let new = copy_with_commit(name, &target, COMMIT);
        resign(&new);
        let differ = std::fs::read(&old).unwrap() != std::fs::read(&new).unwrap();
        assert!(differ, "signing {name} again left its bytes as they were; the case is not built");
    }
    (running, target)
}

#[test]
fn a_second_install_of_the_running_commit_swaps_nothing() {
    let tmp = TempDir::new("install-layers");
    let (running, target) = two_builds(&tmp);
    let line = explain(&running, &target);
    assert_eq!(field(&line, "swap"), "none", "{line}");
    assert_eq!(
        field(&line, "why"),
        "shell:same-commit,core:same-commit,session:same-commit,peer:same-commit",
        "{line}"
    );
}

#[test]
fn a_peer_from_another_commit_is_replaced() {
    let tmp = TempDir::new("install-layers");
    let (running, target) = two_builds(&tmp);
    copy_with_commit("marspot-peer", &target, "0ddba11a");
    let line = explain(&running, &target);
    assert_eq!(field(&line, "swap"), "peer", "{line}");
    assert!(field(&line, "why").ends_with(",peer:bytes-differ"), "{line}");
}

/// A `-dirty` build names no commit: only identical bytes are kept.
#[test]
fn a_dirty_peer_is_compared_by_its_bytes() {
    let tmp = TempDir::new("install-layers");
    let (running, target) = two_builds(&tmp);
    let dirty = format!("{COMMIT}-dirty");
    let old = copy_with_commit("marspot-peer", &running, &dirty);
    let new = copy_with_commit("marspot-peer", &target, &dirty);
    assert!(field(&explain(&running, &target), "why").ends_with(",peer:bytes-same"));
    resign(&new);
    assert_ne!(std::fs::read(&old).unwrap(), std::fs::read(&new).unwrap());
    let line = explain(&running, &target);
    assert!(field(&line, "why").ends_with(",peer:bytes-differ"), "{line}");
    assert!(field(&line, "swap").ends_with("peer"), "{line}");
}
