//! Binary-slot manager for the silent-update tree.
//!
//! Lives in `marspot-term` rather than `marspot-shell` because both the
//! shell (its own self-update + the core swap) and the daemon (`marspot-
//! shelld` self-update via the execv handoff) share the same on-disk
//! layout — three slots plus quarantine, under
//! `~/Library/Caches/marspot/binaries/`:
//!
//! ```text
//!   current/<bin_name>    ← what we just exec'd, or are about to
//!   prev/<bin_name>       ← the version before that (rollback target)
//!   pending/<bin_name>    ← the updater dropped a new one here
//!   quarantine/<bin_name> ← a binary that failed probation
//! ```
//!
//! The shell-side state machine that drives swaps + probation stays in
//! `marspot-shell/supervisor.rs`; this module is just the filesystem
//! surface so shelld can `has_pending()` / `promote_pending()` from a
//! crate that does not pull in any GUI dependencies.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `bin --version` to completion and report whether it exited 0.
///
/// Two things ride on one cheap fork, and both matter:
///
/// 1. **It proves the image can run at all.**  On 2026-07-26 an
///    adhoc-signed binary reached `pending/`, AMFI killed the successor,
///    and because `exec` had already replaced the caller there was no
///    process left to notice — no window, no log line, every live
///    session orphaned.
/// 2. **It pays the Gatekeeper bill early.**  macOS assesses a newly
///    created executable on its *first* exec, and that assessment has no
///    upper bound: on 2026-07-29 a concurrent cargo build flooded
///    `syspolicyd` and `marspot-core` sat in the kernel's exec path for
///    204 s.  Doing it here means the wait happens while the process
///    being replaced is still serving, instead of inside the gap after
///    it has been retired.
///
/// Point 2 only holds because `promote_pending` moves the file with
/// `rename`, which preserves the inode the verdict is cached against —
/// probe `pending/`, spawn `current/`, same file, cached answer.
///
/// `MARSPOT_NO_REDIRECT` stops a probed shell from bouncing into
/// `current/`: the whole point is to test *this* file.
///
/// Callers must run this off whatever thread is keeping the UI alive.
/// How long the candidate gets to answer `--version`.
///
/// It is a print and an exit; anything longer is the system taking its
/// time (a cold Gatekeeper verdict) or not answering at all.  Waiting
/// forever is the worse failure: the caller treats "probe outstanding"
/// as "do not probe again", so one hung check retires that pane's
/// self-update permanently and says nothing (2026-09-06).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub fn can_start(bin: &Path) -> bool {
    can_start_within(bin, PROBE_TIMEOUT)
}

/// [`can_start`] with the deadline spelled out, so a test can prove the
/// bound exists without waiting for it.
pub fn can_start_within(bin: &Path, timeout: std::time::Duration) -> bool {
    match Command::new(bin)
        .arg("--version")
        .env("MARSPOT_NO_REDIRECT", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(st)) if st.success() => return true,
                    Ok(Some(st)) => {
                        crate::lx_warn!(
                            "binary_tree.can_start.exited",
                            &format!("{} answered {st}", bin.display())
                        );
                        return false;
                    }
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            crate::lx_warn!(
                                "binary_tree.can_start.timed_out",
                                &format!(
                                    "{} did not answer --version in {timeout:?}",
                                    bin.display()
                                )
                            );
                            return false;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(e) => {
                        crate::lx_warn!(
                            "binary_tree.can_start.wait_failed",
                            &format!("{}: {e}", bin.display())
                        );
                        return false;
                    }
                }
            }
        }
        // A verdict with no reason is what made this expensive to
        // chase: every L3 refused to adopt a perfectly good image for
        // half an hour and all the log said was "could not start"
        // (2026-09-06).  The two cases are not alike — a non-zero exit
        // is the binary answering, a spawn error is the system
        // refusing — and only one of them means the binary is bad.
        Err(e) => {
            crate::lx_warn!(
                "binary_tree.can_start.spawn_failed",
                &format!("{}: {e}", bin.display())
            );
            false
        }
    }
}

/// macOS-specific. Strip `com.apple.quarantine` and
/// `com.apple.provenance` so the binary is launchable without a
/// synchronous Gatekeeper check stall. Best-effort: silently
/// ignores missing xattrs.
fn strip_quarantine_xattrs(path: &Path) {
    for attr in &["com.apple.quarantine", "com.apple.provenance"] {
        let _ = Command::new("/usr/bin/xattr")
            .args(["-d", attr])
            .arg(path)
            .output();
    }
}

/// All three binary slots (+ quarantine) for one artifact. Generic over
/// the binary name so the same machinery serves all three layers:
/// `marspot-shelld`, `marspot-shell`, and `marspot-core`.
pub struct BinaryTree {
    root: PathBuf,
    bin_name: String,
}

impl BinaryTree {
    /// Constructs a tree rooted at the active state dir's `binaries/`
    /// (`crate::paths::binaries_root` — honours `MARSPOT_STATE_DIR` so
    /// a dev / test sandbox swaps binaries in its own tree).
    pub fn default_for(bin_name: impl Into<String>) -> io::Result<Self> {
        Ok(BinaryTree {
            root: crate::paths::binaries_root(),
            bin_name: bin_name.into(),
        })
    }

    /// Convenience: tree for the renderer.
    pub fn for_core() -> io::Result<Self> {
        Self::default_for("marspot-core")
    }
    /// Convenience: tree for the shell supervisor itself. Used by the
    /// shell self-update path — promote pending → current, then exec
    /// the new shell over ourselves.
    pub fn for_shell() -> io::Result<Self> {
        Self::default_for("marspot-shell")
    }
    /// Convenience: tree for the daemon. Promote happens either via
    /// `bin/install-shelld.sh --apply-pending` (bootout/bootstrap
    /// path, kills sessions) or via the shelld execv handoff path
    /// (SIGUSR1, sessions survive).
    pub fn for_shelld() -> io::Result<Self> {
        Self::default_for("marspot-shelld")
    }

    pub fn current(&self) -> PathBuf {
        self.root.join("current").join(&self.bin_name)
    }
    pub fn prev(&self) -> PathBuf {
        self.root.join("prev").join(&self.bin_name)
    }
    pub fn pending(&self) -> PathBuf {
        self.root.join("pending").join(&self.bin_name)
    }

    /// True iff the updater has staged a new binary in `pending/`.
    pub fn has_pending(&self) -> bool {
        self.pending().exists()
    }

    /// Resolve which binary to `exec`. Preference order:
    ///   1. `MARSPOT_CORE_BIN` env override (full path) — historical
    ///      name kept for compatibility with the existing core path.
    ///   2. `current/<bin_name>` if present.
    ///   3. `MARSPOT_BUNDLE_DIR` fallback (set pre-exec so the shell's
    ///      self-update successor can still find core).
    ///   4. Sibling of the fallback path (dev / first-run install).
    pub fn resolve_runnable(&self, fallback_sibling: &Path) -> PathBuf {
        if let Some(env) = std::env::var_os("MARSPOT_CORE_BIN") {
            let p = PathBuf::from(env);
            if p.is_absolute() && p.exists() {
                return p;
            }
            return fallback_sibling.with_file_name(p);
        }
        let cur = self.current();
        if cur.exists() {
            return cur;
        }
        if let Some(bundle) = std::env::var_os("MARSPOT_BUNDLE_DIR") {
            let p = PathBuf::from(bundle).join(&self.bin_name);
            if p.exists() {
                return p;
            }
        }
        fallback_sibling.with_file_name(&self.bin_name)
    }

    /// Move `current → prev` and `pending → current`, atomically per
    /// slot. If either rename fails the tree is left in a state the
    /// caller can rollback from (either everything still on current,
    /// or pending consumed but current missing — the caller's
    /// responsibility to react).
    ///
    /// Pre-condition: `pending/<bin_name>` exists.
    pub fn promote_pending(&self) -> io::Result<()> {
        if !self.pending().exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no pending binary to promote",
            ));
        }
        for slot in ["current", "prev"] {
            std::fs::create_dir_all(self.root.join(slot))?;
        }
        let cur = self.current();
        let prev = self.prev();
        if cur.exists() {
            std::fs::rename(&cur, &prev)?;
        }
        std::fs::rename(self.pending(), &cur)?;
        // Strip Gatekeeper / provenance xattrs. Without this the first
        // launch of the newly-promoted binary stalls in `_dyld_start`
        // for 30-60 s during a synchronous provenance check — fatal
        // for a silent upgrade. Defence in depth: the updater also
        // strips at stage time, but a manually-copied pending binary
        // won't have been.
        strip_quarantine_xattrs(&cur);
        Ok(())
    }

    /// Move `pending → quarantine`, without touching `current`.
    ///
    /// For a candidate rejected *before* it was ever promoted — the
    /// caller probed it, found it cannot start, and must not exec into
    /// it.  Leaving it in `pending/` would make the next update trigger
    /// retry the same dead binary forever; `quarantine/` keeps it
    /// around for inspection, which is the whole point of that slot.
    pub fn quarantine_pending(&self) -> io::Result<()> {
        let pending = self.pending();
        if !pending.exists() {
            return Ok(());
        }
        let quar = self.root.join("quarantine").join(&self.bin_name);
        if let Some(parent) = quar.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(pending, quar)
    }

    /// Move `prev → current`, overwriting whatever is there. Quarantines
    /// the existing current (the failed binary) for crash-report
    /// retention.
    ///
    ///   - If `prev/` has a binary, returns `Ok(true)` (rolled back to a
    ///     known-good).
    ///   - If `prev/` is empty, returns `Ok(false)` — caller falls back
    ///     to the sibling path (whatever the supervisor was originally
    ///     exec'd next to).
    pub fn rollback_to_prev(&self) -> io::Result<bool> {
        let cur = self.current();
        let quar = self.root.join("quarantine").join(&self.bin_name);
        if cur.exists() {
            if let Some(parent) = quar.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&cur, &quar);
        }
        if !self.prev().exists() {
            return Ok(false);
        }
        std::fs::rename(self.prev(), &cur)?;
        Ok(true)
    }

    /// Delete `prev/` after the new binary has cleared probation.
    pub fn finalize_stable(&self) -> io::Result<()> {
        let prev = self.prev();
        if prev.exists() {
            std::fs::remove_file(prev)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 探测只认「跑起来并且退 0」。退非 0 / 根本不存在 / 不可执行,
    /// 都必须判定为不可用 —— 这是 execv 之前唯一一道闸。
    #[test]
    fn can_start_accepts_only_a_clean_zero_exit() {
        assert!(can_start(Path::new("/usr/bin/true")), "exit 0 must pass");
        assert!(!can_start(Path::new("/usr/bin/false")), "exit 1 must fail");
        assert!(
            !can_start(Path::new("/nonexistent/marspot-core")),
            "a missing file must fail, not panic"
        );
        assert!(
            !can_start(Path::new("/etc/hosts")),
            "a non-executable must fail"
        );
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(p).unwrap();
        writeln!(f, "test").unwrap();
    }

    fn temp_tree(bin: &str) -> (tempdir_lite::Dir, BinaryTree) {
        let dir = tempdir_lite::Dir::new();
        let tree = BinaryTree {
            root: dir.path().to_path_buf(),
            bin_name: bin.to_string(),
        };
        (dir, tree)
    }

    #[test]
    fn has_pending_reflects_filesystem() {
        let (_d, tree) = temp_tree("marspot-core");
        assert!(!tree.has_pending());
        touch(&tree.pending());
        assert!(tree.has_pending());
    }

    #[test]
    fn promote_pending_moves_slots() {
        let (_d, tree) = temp_tree("marspot-core");
        touch(&tree.current());
        touch(&tree.pending());
        tree.promote_pending().unwrap();
        assert!(tree.current().exists());
        assert!(tree.prev().exists());
        assert!(!tree.pending().exists());
    }

    #[test]
    fn promote_fresh_install() {
        let (_d, tree) = temp_tree("marspot-core");
        touch(&tree.pending());
        tree.promote_pending().unwrap();
        assert!(tree.current().exists());
        assert!(!tree.prev().exists());
    }

    #[test]
    fn rollback_restores_prev() {
        let (_d, tree) = temp_tree("marspot-core");
        touch(&tree.current());
        touch(&tree.prev());
        std::fs::write(tree.current(), b"new").unwrap();
        std::fs::write(tree.prev(), b"old").unwrap();
        assert_eq!(tree.rollback_to_prev().unwrap(), true);
        assert_eq!(std::fs::read(tree.current()).unwrap(), b"old");
        assert!(tree.root.join("quarantine").join("marspot-core").exists());
    }

    #[test]
    fn rollback_without_prev_quarantines_current() {
        let (_d, tree) = temp_tree("marspot-core");
        touch(&tree.current());
        std::fs::write(tree.current(), b"new-but-broken").unwrap();
        assert_eq!(tree.rollback_to_prev().unwrap(), false);
        assert!(!tree.current().exists());
        let quar = tree.root.join("quarantine").join("marspot-core");
        assert!(quar.exists());
        assert_eq!(std::fs::read(quar).unwrap(), b"new-but-broken");
    }

    #[test]
    fn finalize_deletes_prev() {
        let (_d, tree) = temp_tree("marspot-core");
        touch(&tree.current());
        touch(&tree.prev());
        tree.finalize_stable().unwrap();
        assert!(tree.current().exists());
        assert!(!tree.prev().exists());
    }

    #[test]
    fn shelld_tree_uses_shelld_bin_name() {
        // for_shelld() must point at binaries/{current,prev,pending}/marspot-shelld
        // — the boot-promote path on the daemon reads from this exact location.
        let dir = tempdir_lite::Dir::new();
        let tree = BinaryTree {
            root: dir.path().to_path_buf(),
            bin_name: "marspot-shelld".to_string(),
        };
        assert!(tree.current().ends_with("current/marspot-shelld"));
        assert!(tree.pending().ends_with("pending/marspot-shelld"));
        assert!(tree.prev().ends_with("prev/marspot-shelld"));
    }
}

#[cfg(test)]
mod tempdir_lite {
    use std::path::{Path, PathBuf};

    pub struct Dir {
        path: PathBuf,
    }

    impl Dir {
        pub fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let p = std::env::temp_dir().join(format!("marspot-bt-test-{pid}-{n}"));
            std::fs::create_dir_all(&p).unwrap();
            Dir { path: p }
        }
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod probe_bound_tests {
    use super::can_start;

    /// A candidate that never answers must not hold the caller.
    ///
    /// The caller reads "probe outstanding" as "already handled", so an
    /// unbounded wait here does not merely delay one check — it retires
    /// that pane's self-update for the life of the process, silently.
    ///
    /// The candidate has to genuinely hang.  `/bin/sleep --version`
    /// does not: it rejects the argument and exits at once, taking the
    /// "answered non-zero" path and proving nothing about the bound.
    #[test]
    fn a_candidate_that_never_answers_is_given_up_on() {
        let script =
            std::env::temp_dir().join(format!("marspot-probe-hang-{}", std::process::id()));
        std::fs::write(&script, b"#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(
            &script,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();

        let budget = std::time::Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        let verdict = super::can_start_within(&script, budget);
        let took = t0.elapsed();
        std::fs::remove_file(&script).ok();

        assert!(
            !verdict,
            "a candidate that does not answer is not startable"
        );
        assert!(
            took < budget * 8,
            "gave up after {took:?} against a {budget:?} budget — not a bound"
        );
    }

    /// A candidate that answers non-zero is rejected at once, not
    /// waited out.
    #[test]
    fn a_candidate_that_refuses_is_rejected_immediately() {
        let t0 = std::time::Instant::now();
        assert!(!can_start(std::path::Path::new("/usr/bin/false")));
        assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    }

    /// And a real binary still passes — the bound must not have turned
    /// the check into "always no".
    #[test]
    fn a_working_binary_still_passes() {
        assert!(can_start(std::path::Path::new("/usr/bin/true")));
    }
}
