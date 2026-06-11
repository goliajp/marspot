//! marspot-bootstrap — tiny "trampoline" binary that the .app
//! bundle's `CFBundleExecutable` points at.
//!
//! Job is small but load-bearing for silent updates:
//!
//! 1. If `~/Library/Caches/marspot/pending/marspot` exists, swap it
//!    in: move the current `.app/Contents/MacOS/marspot` aside to
//!    `~/Library/Caches/marspot/previous/marspot` and rename
//!    pending into place atomically.
//! 2. `execv` the real marspot binary, replacing this process so the
//!    user sees a marspot window (not a no-op bootstrap process).
//! 3. If marspot exits within `STARTUP_GRACE_SECS`, treat that as a
//!    bad update and roll back to the previous binary, then re-exec.
//!
//! This is the safety net for "silent update applied a broken
//! binary": users can't get into a state where double-clicking the
//! .app does nothing.
//!
//! The shim must stay self-contained.  Anything that depends on
//! marspot's lib could break the rollback (a broken lib changed in
//! the failed update could prevent the shim from running).  We use
//! only `std` here.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// If marspot exits within this window after exec, the update is
/// presumed bad and we roll back to the previous binary.
const STARTUP_GRACE_SECS: u64 = 5;

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Caches/marspot")
}

fn marspot_binary_path() -> PathBuf {
    // We're at .app/Contents/MacOS/marspot-bootstrap; the real binary
    // sits next to us.  Resolve by reading our own argv[0] and
    // swapping the file name.
    let mut me = std::env::current_exe().unwrap_or_else(|_| {
        // Fallback for unusual setups (running outside an .app).
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".local/Marspot.app/Contents/MacOS/marspot-bootstrap")
    });
    me.set_file_name("marspot");
    me
}

fn pending_binary() -> PathBuf {
    cache_dir().join("pending/marspot")
}

fn previous_binary() -> PathBuf {
    cache_dir().join("previous/marspot")
}

fn log_line(s: &str) {
    let log_path = cache_dir().join("bootstrap.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write as _;
        let _ = writeln!(f, "[{}] {}", std::process::id(), s);
    }
}

/// Apply a pending update if one is staged.  No-op when there isn't
/// one.  Failures are logged but don't abort — better to keep
/// running the existing binary than to refuse to launch.
fn apply_pending() {
    let pending = pending_binary();
    if !pending.exists() {
        return;
    }
    log_line(&format!("applying pending update from {}", pending.display()));

    let current = marspot_binary_path();
    let prev = previous_binary();
    if let Some(parent) = prev.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 1. Move current → previous (so a failed new binary can roll
    //    back).
    if current.exists() {
        if let Err(e) = std::fs::rename(&current, &prev) {
            log_line(&format!(
                "failed to back up current {} → {}: {} — aborting update",
                current.display(),
                prev.display(),
                e
            ));
            return;
        }
    }

    // 2. Move pending → current.  Atomic on APFS within the same
    //    volume (Caches and the .app may be on different volumes;
    //    if rename fails for that reason, fall back to copy+rename
    //    of the dst).
    let install_err = std::fs::rename(&pending, &current).err().map(|e| {
        if e.kind() == std::io::ErrorKind::CrossesDevices {
            None
        } else {
            Some(e)
        }
    });
    if let Some(Some(e)) = install_err {
        log_line(&format!(
            "rename {} → {} failed: {} — rolling back",
            pending.display(),
            current.display(),
            e
        ));
        let _ = std::fs::rename(&prev, &current);
        return;
    }
    if install_err.is_some() {
        // Cross-device: fall back to copy + remove.
        if let Err(e) = std::fs::copy(&pending, &current) {
            log_line(&format!(
                "copy {} → {} failed: {} — rolling back",
                pending.display(),
                current.display(),
                e
            ));
            let _ = std::fs::rename(&prev, &current);
            return;
        }
        let _ = std::fs::remove_file(&pending);
    }

    // 3. Ensure executable bit.  rename preserves perms; copy may not.
    if let Ok(meta) = std::fs::metadata(&current) {
        let mut p = meta.permissions();
        p.set_mode(0o755);
        let _ = std::fs::set_permissions(&current, p);
    }
    log_line("pending update installed");
}

/// Restore `~/Library/Caches/marspot/previous/marspot` over the
/// current binary.  Called after `STARTUP_GRACE_SECS` if the just-
/// installed binary exited too fast.
fn rollback() -> bool {
    let prev = previous_binary();
    if !prev.exists() {
        log_line("rollback requested but no previous/ backup");
        return false;
    }
    let current = marspot_binary_path();
    log_line(&format!(
        "rolling back {} → {}",
        prev.display(),
        current.display()
    ));
    let _ = std::fs::remove_file(&current);
    if let Err(e) = std::fs::rename(&prev, &current) {
        log_line(&format!("rollback rename failed: {}", e));
        return false;
    }
    if let Ok(meta) = std::fs::metadata(&current) {
        let mut p = meta.permissions();
        p.set_mode(0o755);
        let _ = std::fs::set_permissions(&current, p);
    }
    true
}

fn main() {
    log_line(&format!(
        "bootstrap starting; pid={} args={:?}",
        std::process::id(),
        std::env::args().collect::<Vec<_>>()
    ));
    apply_pending();

    // Spawn the real marspot.  We Command::status (i.e. wait) so we
    // can observe whether it died inside the grace window.  If it
    // runs longer than that, we just exit and leave marspot to its
    // normal lifecycle.
    let bin = marspot_binary_path();
    if !bin.exists() {
        log_line(&format!("real binary missing at {} — rollback", bin.display()));
        if rollback() {
            // Re-launch from the rolled-back binary by retrying.
            if let Err(e) = exec_marspot(&bin) {
                log_line(&format!("post-rollback exec failed: {}", e));
                std::process::exit(1);
            }
        }
        std::process::exit(1);
    }

    let started = Instant::now();
    let result = Command::new(&bin).args(std::env::args().skip(1)).status();
    let elapsed = started.elapsed();

    match result {
        Ok(s) if s.success() && elapsed > Duration::from_secs(STARTUP_GRACE_SECS) => {
            log_line(&format!("marspot exited cleanly after {:?}", elapsed));
        }
        Ok(s) => {
            log_line(&format!(
                "marspot exited with {:?} after {:?}{}",
                s,
                elapsed,
                if elapsed <= Duration::from_secs(STARTUP_GRACE_SECS) {
                    " — within grace, attempting rollback"
                } else {
                    ""
                }
            ));
            if elapsed <= Duration::from_secs(STARTUP_GRACE_SECS) && previous_binary().exists()
            {
                if rollback() {
                    log_line("rolled back; re-executing");
                    let _ = exec_marspot(&bin);
                }
            }
        }
        Err(e) => {
            log_line(&format!("failed to launch marspot: {}", e));
            if previous_binary().exists() && rollback() {
                let _ = exec_marspot(&bin);
            }
            std::process::exit(1);
        }
    }
}

/// Replace this process with the marspot binary.  Returns only on
/// failure.  Used during rollback — after a successful rollback we
/// can re-exec rather than spawn-and-wait, since the rolled-back
/// binary is the same code that was already running.
fn exec_marspot(bin: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Err(Command::new(bin).args(args).exec())
}
