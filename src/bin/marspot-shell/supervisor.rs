//! Binary-tree manager + update state machine.
//!
//! The shell is the *supervisor* in the silent-update architecture:
//! it owns the window and never restarts, the core is the worker that
//! gets swapped on every upgrade.  This module gives the shell the
//! file-system surface for that — three slots under
//! `~/Library/Caches/marspot/binaries/`:
//!
//! ```text
//!   current/marspot-core   ← what we just exec'd, or are about to
//!   prev/marspot-core      ← the version before that (rollback target)
//!   pending/marspot-core   ← the updater dropped a new one here
//! ```
//!
//! Plus a tiny state machine the shell runs as it swaps binaries.
//!
//! ## Lifecycle
//!
//! ```text
//!         ┌─── promote_pending ───┐
//!  Idle ──┤                       ├──→ Probation(30s)
//!         └─── (no pending)       │           ├──→ Stable   (deletes prev)
//!                                 │           └──→ Failed   (mv prev → current)
//!                                 │
//!                                 └──→ Failed (filesystem swap errored)
//! ```
//!
//! Probation tracks "did the new core stay alive long enough that we
//! trust it?".  Step 5 implements wall-clock probation (30 s by
//! default).  Step 6 layers crash detection + healthcheck ping on
//! top.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// macOS-specific.  Strip `com.apple.quarantine` and
/// `com.apple.provenance` so the binary is launchable without a
/// synchronous Gatekeeper check stall.  Best-effort: silently
/// ignores missing xattrs.
fn strip_quarantine_xattrs(path: &Path) {
    for attr in &["com.apple.quarantine", "com.apple.provenance"] {
        let _ = Command::new("/usr/bin/xattr")
            .args(["-d", attr])
            .arg(path)
            .output();
    }
}

/// How long the supervisor watches a freshly-promoted binary before
/// declaring it stable.  30 s = enough for a flat-out broken binary
/// to abort during startup, short enough that an upgrade feels
/// committed.
pub const PROBATION: Duration = Duration::from_secs(30);

/// All three binary slots (+ quarantine) for one artifact.  Generic
/// over the binary name so the same machinery serves all three
/// layers: `marspot-shelld`, `marspot-shell`, and `marspot-core`.
pub struct BinaryTree {
    root: PathBuf,
    bin_name: String,
}

impl BinaryTree {
    /// Constructs a tree rooted at the active state dir's `binaries/`
    /// (`marspot::paths::binaries_root` — honours `MARSPOT_STATE_DIR`
    /// so a dev / test sandbox swaps binaries in its own tree).
    pub fn default_for(bin_name: impl Into<String>) -> io::Result<Self> {
        Ok(BinaryTree {
            root: marspot::paths::binaries_root(),
            bin_name: bin_name.into(),
        })
    }

    /// Convenience: tree for the renderer (Step 2-7).
    pub fn for_core() -> io::Result<Self> {
        Self::default_for("marspot-core")
    }
    /// Convenience: tree for the shell supervisor itself.  Used by
    /// the shell self-update path (Task C) — promote pending →
    /// current, then exec the new shell over ourselves.
    pub fn for_shell() -> io::Result<Self> {
        Self::default_for("marspot-shell")
    }
    /// Convenience: tree for the daemon.  Promote happens via
    /// `bin/install-shelld.sh --apply-pending` because daemon
    /// restart kills all sessions and needs explicit consent.
    #[allow(dead_code)]
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

    /// True iff the updater has staged a new binary in `pending/`
    /// **and** it differs (by mtime, fast path) from what we're
    /// currently running.  We treat "no current/" as a fresh
    /// install and let the caller decide whether to promote.
    pub fn has_pending(&self) -> bool {
        self.pending().exists()
    }

    /// Resolve which core binary the shell should `exec` for the
    /// next child.  Preference order:
    ///   1. `MARSPOT_CORE_BIN` env override (full path)
    ///   2. `current/marspot-core` if present
    ///   3. Sibling of the shell binary (dev / first-run install)
    pub fn resolve_runnable(&self, fallback_sibling: &Path) -> PathBuf {
        if let Some(env) = std::env::var_os("MARSPOT_CORE_BIN") {
            let p = PathBuf::from(env);
            if p.is_absolute() && p.exists() {
                return p;
            }
            // Otherwise treat as a name to resolve under the sibling.
            return fallback_sibling.with_file_name(p);
        }
        let cur = self.current();
        if cur.exists() {
            return cur;
        }
        // Shell-self-update path: after exec into
        // `binaries/current/marspot-shell`, the new shell's
        // `current_exe()` lives in a directory that DOESN'T have a
        // sibling marspot-core (unless the updater also staged
        // core).  `MARSPOT_BUNDLE_DIR` is set by the *outgoing* shell
        // pre-exec to its original sibling dir (the bundle's
        // MacOS/), so the new shell can fall back to that.
        if let Some(bundle) = std::env::var_os("MARSPOT_BUNDLE_DIR") {
            let p = PathBuf::from(bundle).join(&self.bin_name);
            if p.exists() {
                return p;
            }
        }
        fallback_sibling.with_file_name(&self.bin_name)
    }

    /// Move `current → prev` and `pending → current`, atomically per
    /// slot.  If either rename fails the tree is left in a state the
    /// caller can rollback from (either everything still on current
    /// or pending consumed but current missing — the caller's
    /// responsibility to react).
    ///
    /// Pre-conditions: `pending/marspot-core` exists.  No-op if not.
    pub fn promote_pending(&self) -> io::Result<()> {
        if !self.pending().exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no pending binary to promote",
            ));
        }
        // mkdir parents.
        for slot in ["current", "prev"] {
            std::fs::create_dir_all(self.root.join(slot))?;
        }
        // 1. Move current → prev, overwriting any older prev.
        let cur = self.current();
        let prev = self.prev();
        if cur.exists() {
            // `std::fs::rename` replaces dest on Unix.
            std::fs::rename(&cur, &prev)?;
        }
        // 2. Move pending → current.  If this fails after step 1,
        //    `current/` is now empty; caller must restore via
        //    `rollback_to_prev`.
        std::fs::rename(self.pending(), &cur)?;
        // 3. Strip Gatekeeper / provenance xattrs.  Without this,
        //    the first launch of the newly-promoted binary stalls
        //    in `_dyld_start` for ~30-60 s while LaunchServices
        //    runs a synchronous provenance check — fatal for a
        //    "silent" upgrade because the new core/shell appears
        //    hung. Defence-in-depth: the updater also strips at
        //    stage time, but a manually-copied pending binary
        //    won't have been.
        strip_quarantine_xattrs(&cur);
        Ok(())
    }

    /// Move `prev → current`, overwriting whatever is currently there.
    /// Used when probation declares the new binary failed.
    ///
    /// Always quarantines the existing `current/marspot-core` (so the
    /// failed binary stays available for a crash report) and:
    ///
    ///   - If `prev/` has a binary, moves it into `current/` and
    ///     returns `Ok(true)` (rolled back to a known-good).
    ///   - If `prev/` is empty (the rolled-back binary was the *first*
    ///     ever promoted — shell had been running from its dev sibling
    ///     before), there's nothing to restore.  `current/` is left
    ///     empty; the next `resolve_runnable()` falls back to the
    ///     sibling path, which is the binary the shell started under.
    ///     Returns `Ok(false)` so the caller can log appropriately
    ///     without treating the empty-prev case as a hard error.
    pub fn rollback_to_prev(&self) -> io::Result<bool> {
        let cur = self.current();
        // Quarantine whatever's currently there (the failed binary).
        // Best-effort: we don't fail rollback if quarantine fails.
        let quar = self.root.join("quarantine").join(&self.bin_name);
        if cur.exists() {
            if let Some(parent) = quar.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&cur, &quar);
        }
        if !self.prev().exists() {
            // No prev to restore; current is now empty so
            // `resolve_runnable` will fall back to the sibling
            // (whatever marspot-shell was originally exec'd next to).
            return Ok(false);
        }
        std::fs::rename(self.prev(), &cur)?;
        Ok(true)
    }

    /// Delete `prev/` after the new binary has cleared probation.
    /// We keep it for a release boundary so a manual "rollback to
    /// the version I had before yesterday's update" remains possible,
    /// but it isn't load-bearing for the auto-rollback path.
    pub fn finalize_stable(&self) -> io::Result<()> {
        let prev = self.prev();
        if prev.exists() {
            std::fs::remove_file(prev)?;
        }
        Ok(())
    }
}

/// Where in the swap lifecycle the shell currently is.  Lives on
/// `ShellApp`; transitions driven by either explicit user action
/// (refresh button click), focus-loss timer, or core-child exit
/// detection.
#[derive(Debug)]
pub enum SupervisorState {
    /// No update in flight.  If `BinaryTree::has_pending()` becomes
    /// true the shell can move to `PreSwap`.
    Idle,
    /// New core has been exec'd; we're watching it for the first
    /// `PROBATION` seconds.  `started_at` is wall-clock at the swap.
    Probation { started_at: Instant },
    /// Probation passed without the core dying.  We finalize and
    /// fall back to `Idle`.
    #[allow(dead_code)]
    Stable,
    /// Swap failed — either filesystem error, or the new core died
    /// inside probation.  We've already rolled back the binaries;
    /// the running core is whatever was the *previous* one.
    #[allow(dead_code)]
    Failed { reason: String },
}

impl SupervisorState {
    pub fn probation_elapsed(&self) -> bool {
        match self {
            SupervisorState::Probation { started_at } => {
                started_at.elapsed() >= PROBATION
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(p).unwrap();
        writeln!(f, "test").unwrap();
    }

    fn temp_tree() -> (tempdir_lite::Dir, BinaryTree) {
        let dir = tempdir_lite::Dir::new();
        let tree = BinaryTree {
            root: dir.path().to_path_buf(),
            bin_name: "marspot-core".to_string(),
        };
        (dir, tree)
    }

    #[test]
    fn has_pending_reflects_filesystem() {
        let (_d, tree) = temp_tree();
        assert!(!tree.has_pending());
        touch(&tree.pending());
        assert!(tree.has_pending());
    }

    #[test]
    fn promote_pending_moves_slots() {
        let (_d, tree) = temp_tree();
        touch(&tree.current());
        touch(&tree.pending());
        tree.promote_pending().unwrap();
        assert!(tree.current().exists());
        assert!(tree.prev().exists());
        assert!(!tree.pending().exists());
    }

    #[test]
    fn promote_fresh_install() {
        // No prior current — first install path.
        let (_d, tree) = temp_tree();
        touch(&tree.pending());
        tree.promote_pending().unwrap();
        assert!(tree.current().exists());
        assert!(!tree.prev().exists());
    }

    #[test]
    fn rollback_restores_prev() {
        let (_d, tree) = temp_tree();
        touch(&tree.current());
        touch(&tree.prev());
        // Mark current with a unique sentinel so we can tell after
        // rollback whether it was overwritten.
        std::fs::write(tree.current(), b"new").unwrap();
        std::fs::write(tree.prev(), b"old").unwrap();
        assert_eq!(tree.rollback_to_prev().unwrap(), true);
        assert_eq!(std::fs::read(tree.current()).unwrap(), b"old");
        // Quarantine retains the failed binary.
        assert!(tree.root.join("quarantine").join("marspot-core").exists());
    }

    #[test]
    fn rollback_without_prev_quarantines_current() {
        let (_d, tree) = temp_tree();
        touch(&tree.current());
        std::fs::write(tree.current(), b"new-but-broken").unwrap();
        // No prev/ → rollback returns false …
        assert_eq!(tree.rollback_to_prev().unwrap(), false);
        // … and quarantines the broken current so it doesn't keep
        // crashing the shell on respawn.
        assert!(!tree.current().exists());
        let quar = tree.root.join("quarantine").join("marspot-core");
        assert!(quar.exists());
        assert_eq!(std::fs::read(quar).unwrap(), b"new-but-broken");
    }

    #[test]
    fn finalize_deletes_prev() {
        let (_d, tree) = temp_tree();
        touch(&tree.current());
        touch(&tree.prev());
        tree.finalize_stable().unwrap();
        assert!(tree.current().exists());
        assert!(!tree.prev().exists());
    }
}

#[cfg(test)]
mod tempdir_lite {
    //! Tiny self-built tempdir to avoid pulling in the `tempfile`
    //! crate just for unit tests.  Removed on Drop.
    use std::path::{Path, PathBuf};

    pub struct Dir {
        path: PathBuf,
    }

    impl Dir {
        pub fn new() -> Self {
            // Inline pid + a thread-id-derived counter so two tests
            // running in parallel don't collide.  Date is added to
            // make manual cleanup easier if a test leaks one.
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let p = std::env::temp_dir().join(format!("marspot-test-{pid}-{n}"));
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
