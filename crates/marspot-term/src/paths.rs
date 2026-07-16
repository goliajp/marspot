//! Single source of truth for marspot's on-disk state locations.
//!
//! `MARSPOT_STATE_DIR` overrides the root.  That one knob is what
//! separates the *installed* app (default root, the terminal the
//! user lives in) from every dev / test world: `bin/test-*.sh` and
//! `bin/run.sh` export their own state dir, run their own shelld on
//! their own socket, and can kill processes / wipe state freely
//! without ever touching the production instance.
//!
//! Everything that names a path under the state root MUST come
//! through here — a hardcoded `~/Library/Caches/marspot` anywhere
//! else is a sandbox escape.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// State root: `$MARSPOT_STATE_DIR`, else
/// `~/Library/Application Support/marspot` (the installed app's
/// world).
///
/// RFC-004 D.1 — moved OUT of `~/Library/Caches/marspot`: macOS
/// treats Caches as purgeable under disk pressure (and cleaner
/// tools wipe it wholesale), which is no home for the user's
/// terminal history.  `migrate_legacy_state_root()` renames the old
/// root here once and leaves a symlink behind, so live processes
/// (open fds survive the rename) and not-yet-recompiled binaries
/// (paths resolve through the symlink) never notice.
pub fn state_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("MARSPOT_STATE_DIR") {
        return PathBuf::from(dir);
    }
    home().join("Library/Application Support/marspot")
}

/// RFC-004 D.1 — one-time migration `~/Library/Caches/marspot` →
/// `~/Library/Application Support/marspot`.  Call at every binary's
/// earliest entry (before logx / any path use).  Idempotent + cheap
/// (two lstats on the steady state).  Rules:
///
///   - `MARSPOT_STATE_DIR` set (dev/test sandbox) → no-op
///   - new root already a real dir → done (steady state)
///   - old root a real dir (not the symlink we leave behind) →
///     `rename(old, new)` + `symlink(new, old)`.  rename is atomic
///     on the same volume; open fds keep working; stragglers that
///     still compute the old path resolve through the symlink.
///   - neither exists → nothing to migrate (fresh install; dirs are
///     created lazily by whoever needs them)
///
/// The symlink step failing is non-fatal (data already safe at the
/// new root); worst case an OLD binary recreates a plain dir at the
/// old path and its writes land in a parallel world until the next
/// image swap — the same exposure every migration scheme has for
/// un-upgraded writers, accepted.
pub fn migrate_legacy_state_root() {
    if std::env::var_os("MARSPOT_STATE_DIR").is_some() {
        return;
    }
    let new_root = state_root();
    let old_root = home().join("Library/Caches/marspot");
    // symlink_metadata: never follow — the post-migration old path
    // IS a symlink and must read as "already migrated".
    let old_is_real_dir = std::fs::symlink_metadata(&old_root)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let new_exists = std::fs::symlink_metadata(&new_root).is_ok();
    if !old_is_real_dir || new_exists {
        return;
    }
    // 2026-07-17 现场教训 — process barrier.  Renaming the root
    // while OTHER marspot processes are alive is unsafe in the
    // mixed-image window: an old-image L3 that recreates a path
    // under the old root (create_dir_all on entry rewrite) would
    // fork the state tree.  Only the true cold-boot first process
    // migrates: if any other marspot-shell/-core/-session is
    // running, skip — a later boot gets it.  (Racing NEW processes
    // are also serialized by this: whoever sees a sibling defers.)
    if other_marspot_processes_alive() {
        return;
    }
    migrate_roots(&old_root, &new_root);
}

/// Barrier-free migration body — separated so tests can exercise the
/// rename+symlink mechanics while a real marspot app is running on
/// the machine (the barrier would otherwise always defer under test).
fn migrate_roots(old_root: &std::path::Path, new_root: &std::path::Path) {
    if let Some(parent) = new_root.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(old_root, new_root) {
        Ok(()) => {
            let _ = std::os::unix::fs::symlink(new_root, old_root);
        }
        Err(e) => {
            // Keep running on the old root via the unchanged
            // computed paths?  No — state_root() already answers the
            // NEW path.  A failed rename with no new dir leaves
            // lazy create_dir_all to start fresh at the new root,
            // which silently forks the user's state.  Fall back HARD:
            // symlink new → old so both names alias the surviving
            // data until a later boot can migrate for real.
            let _ = std::os::unix::fs::symlink(old_root, new_root);
            eprintln!(
                "[marspot] state-root migration failed ({e}); \
                 aliased {} → {}",
                new_root.display(),
                old_root.display()
            );
        }
    }
}

/// Log directory.  The installed app logs to the conventional
/// `~/Library/Logs/Marspot`; sandboxed runs keep logs inside their
/// state root so one `rm -rf` cleans everything.
pub fn log_dir() -> PathBuf {
    if std::env::var_os("MARSPOT_STATE_DIR").is_some() {
        return state_root().join("logs");
    }
    home().join("Library/Logs/Marspot")
}

pub fn supervisor_log() -> PathBuf {
    log_dir().join("supervisor.log")
}

/// Per-session storage root (RFC-003: each L3 owns one subdirectory
/// with its bytelog + entry.toml + sock).
pub fn sessions_dir() -> PathBuf {
    state_root().join("sessions")
}

/// Supervisor binary slots (current/prev/pending/quarantine).
pub fn binaries_root() -> PathBuf {
    state_root().join("binaries")
}

/// Shell crash-loop launch journal.
pub fn shell_launch_journal() -> PathBuf {
    state_root().join("shell_launches.tsv")
}

/// Running GUI shell's pid file — lets `--trigger` / `--status` /
/// scripts target the shell of *this* state root instead of
/// whichever marspot-shell process `ps` happens to list first.
pub fn shell_pid_file() -> PathBuf {
    state_root().join("shell.pid")
}

/// Any marspot process other than this one alive?  Scans the BSD
/// process table via sysctl-free libproc (proc_listallpids +
/// proc_pidpath) — no fork, no shell.  Conservative: enumeration
/// failure reads as "somebody might be alive" so the migration
/// defers rather than racing.
fn other_marspot_processes_alive() -> bool {
    let me = std::process::id() as i32;
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return true;
    }
    let mut pids = vec![0i32; n as usize * 2];
    let filled = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if filled <= 0 {
        return true;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for &pid in pids.iter().take(filled as usize) {
        if pid <= 0 || pid == me {
            continue;
        }
        let len = unsafe {
            libc::proc_pidpath(
                pid,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        if len <= 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&buf[..len as usize]);
        let name = path.rsplit('/').next().unwrap_or("");
        if name.starts_with("marspot-shell")
            || name.starts_with("marspot-core")
            || name.starts_with("marspot-session")
            || name == "marspot"
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_is_under_application_support() {
        if std::env::var_os("MARSPOT_STATE_DIR").is_none() {
            assert!(
                state_root().ends_with("Library/Application Support/marspot")
            );
            assert!(log_dir().ends_with("Library/Logs/Marspot"));
        }
    }

    /// RFC-004 D.1 — migration renames the legacy Caches root to the
    /// new location and leaves a symlink; idempotent on re-run.
    /// Uses a fake $HOME (nextest = process-per-test; safe to set).
    #[test]
    fn migrate_legacy_root_renames_and_symlinks() {
        let fake_home = std::env::temp_dir().join(format!(
            "marspot-d1-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&fake_home);
        let old = fake_home.join("Library/Caches/marspot");
        std::fs::create_dir_all(old.join("sessions/7")).unwrap();
        std::fs::write(old.join("sessions/7/bytelog"), b"H").unwrap();
        // SAFETY: test process, no concurrent env readers.
        unsafe {
            std::env::remove_var("MARSPOT_STATE_DIR");
            std::env::set_var("HOME", &fake_home);
        }
        let new = fake_home.join("Library/Application Support/marspot");
        migrate_roots(&old, &new);
        assert!(
            std::fs::symlink_metadata(&new).unwrap().is_dir(),
            "new root must be a real dir"
        );
        assert_eq!(
            std::fs::read(new.join("sessions/7/bytelog")).unwrap(),
            b"H",
            "content must travel"
        );
        assert!(
            std::fs::symlink_metadata(&old)
                .unwrap()
                .file_type()
                .is_symlink(),
            "old path must be a symlink"
        );
        // Old path still resolves to the same content (straggler
        // binaries keep working through the alias).
        assert_eq!(std::fs::read(old.join("sessions/7/bytelog")).unwrap(), b"H");
        // Idempotent at the public entry (barrier + exists-check):
        migrate_legacy_state_root();
        assert!(std::fs::symlink_metadata(&new).unwrap().is_dir());
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn children_live_under_root() {
        let root = state_root();
        for p in [
            sessions_dir(),
            binaries_root(),
            shell_launch_journal(),
            shell_pid_file(),
        ] {
            assert!(p.starts_with(&root), "{p:?} escapes {root:?}");
        }
    }
}
