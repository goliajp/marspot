//! The install script's answer to "is this pane on the new image" must
//! be the session's own: the build in its `entry.toml` against the
//! staged binary's fingerprint, not the binary's inode.
//!
//! The entries are written by the registry writer and the binary is
//! the real L3, so a change to either format shows up here.

use std::path::{Path, PathBuf};
use std::process::Command;

use marspot_term::session_registry::{SessionEntry, session_entry_path, write_session_entry};
use marspot_term::test_env::{TempDir, sandbox_state_dir};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin/_l3-adoption.sh")
}

/// The L3 binary copied to a new file, which is a new inode.
fn staged_copy(dir: &TempDir) -> PathBuf {
    let staged = dir.join("marspot-session");
    std::fs::copy(env!("CARGO_BIN_EXE_marspot-session"), &staged).unwrap();
    staged
}

fn inode(p: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap())
}

fn entry_with(id: u64, build: &str) -> PathBuf {
    let entry = SessionEntry {
        id,
        pid: 4242,
        socket: PathBuf::from("/nonexistent/sock"),
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: "/".into(),
        proto_version: 2,
        created_at_unix: 1,
        shm_name: String::new(),
        shell_child_pid: 0,
        build: build.into(),
        stamps_regions: true,
        shadow_of: 0,
    };
    write_session_entry(&entry).unwrap();
    session_entry_path(id)
}

/// The script's word for one entry against one binary.
fn verdict(entry: &Path, binary: &Path) -> String {
    let out = Command::new("bash")
        .arg(script())
        .args(["needs-update".as_ref(), entry.as_os_str(), binary.as_os_str()])
        .output()
        .unwrap();
    let word = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let expected_status = if word == "update" { Some(0) } else { Some(1) };
    assert_eq!(out.status.code(), expected_status, "{word:?}, stderr: {}", String::from_utf8_lossy(&out.stderr));
    word
}

#[test]
fn same_build_in_a_new_inode_is_not_reported() {
    let _state = sandbox_state_dir();
    let dir = TempDir::new("adoption");
    let staged = staged_copy(&dir);
    let original = Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    assert_ne!(inode(&staged), inode(original), "the copy is another inode");
    let build = marspot_term::build::of_file(original).expect("the L3 carries its build");
    assert_eq!(verdict(&entry_with(1, &build), &staged), "current");
}

#[test]
fn an_older_build_is_reported() {
    let _state = sandbox_state_dir();
    let dir = TempDir::new("adoption");
    let staged = staged_copy(&dir);
    let entry = entry_with(2, "MARSPOT_FP=0ld0b11d|2026-01-01T00:00:00Z");
    assert_eq!(verdict(&entry, &staged), "update");
}

/// An L3 from before the field is asked with SIGTERM: not on the build.
#[test]
fn an_entry_without_a_build_is_reported() {
    let _state = sandbox_state_dir();
    let dir = TempDir::new("adoption");
    let staged = staged_copy(&dir);
    assert_eq!(verdict(&entry_with(3, ""), &staged), "update");
}

/// Nothing readable staged: the core asks no one, so no pane is behind.
#[test]
fn a_staged_binary_without_a_build_reports_nothing() {
    let _state = sandbox_state_dir();
    let dir = TempDir::new("adoption");
    let blank = dir.join("blank");
    std::fs::write(&blank, b"\x7fELF no marker here").unwrap();
    let entry = entry_with(4, "MARSPOT_FP=0ld0b11d|2026-01-01T00:00:00Z");
    assert_eq!(verdict(&entry, &blank), "current");
}

fn entry_of_pid(root: &Path, pid: u32) -> String {
    let out = Command::new("bash")
        .arg(script())
        .args(["entry-of-pid".as_ref(), root.as_os_str(), pid.to_string().as_ref()])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// After an execv handoff the L3 writes a fresh entry; the newest one
/// for the pid is the one that says which build it runs.
#[test]
fn the_newest_entry_of_a_pid_is_the_one_checked() {
    let _state = sandbox_state_dir();
    let old = entry_with(5, "MARSPOT_FP=0ld0b11d|2026-01-01T00:00:00Z");
    let new = entry_with(6, "MARSPOT_FP=0ld0b11d|2026-01-01T00:00:00Z");
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(past).unwrap();
    let root = new.parent().and_then(Path::parent).and_then(Path::parent).unwrap();
    assert_eq!(entry_of_pid(root, 4242), new.to_string_lossy());
    assert_eq!(entry_of_pid(root, 4243), "", "a pid with no entry is not this app's");
}
