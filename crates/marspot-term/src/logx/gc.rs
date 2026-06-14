//! Cold-data garbage collection.
//!
//! Two entry points:
//! - `sweep_startup(component)` — every binary calls this once at the
//!   end of `logx::init()` in a detached thread. Only touches the shared
//!   log directory's rotated structured logs; cheap and idempotent.
//! - `sweep_full(&LiveSet)` — only shelld calls this from a periodic
//!   tick (every 6 h). Adds the authoritative cross-cutting work:
//!   orphan session bytelogs, age-capped `binaries/{prev,quarantine}`,
//!   and tail-trim of the launchd-managed `shelld.{log,err}` files.
//!
//! The active `marspot.log` is NEVER touched: the rotated-file glob
//! requires a `marspot.<digits>.` prefix, and the active file is plain
//! `marspot.log`. Orphan bytelogs go through a 1 h mtime grace window
//! so a session created mid-sweep doesn't get unlinked.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::sink::log_dir;

const DEFAULT_AGE_DAYS: u64 = 7;
const PREV_AGE_DAYS: u64 = 7;
const QUARANTINE_AGE_DAYS: u64 = 30;
const BYTELOG_GRACE_SECS: u64 = 3600;
const LAUNCHD_LOG_TRIM_BYTES: u64 = 16 * 1024 * 1024;
const LAUNCHD_LOG_RETAIN_BYTES: u64 = 1024 * 1024;

/// shelld's authoritative set of currently-alive session ids. Other
/// components pass an empty `LiveSet`; they only sweep the log dir.
pub struct LiveSet {
    pub session_ids: HashSet<u64>,
}

impl LiveSet {
    pub fn empty() -> Self {
        Self {
            session_ids: HashSet::new(),
        }
    }
}

/// Light startup sweep: only the shared log dir's rotated structured
/// logs. Safe to call from any binary; shelld additionally calls
/// `sweep_full` periodically (see its event loop).
pub fn sweep_startup(_component: &'static str) {
    let dir = log_dir();
    let age = age_threshold();
    sweep_rotated_logs(&dir, age);
}

/// Full sweep — only shelld runs this. `live` is the in-process Sessions
/// map's id set, snapshotted before the sweep so cross-thread races
/// can't lose ids.
pub fn sweep_full(live: &LiveSet) {
    let dir = log_dir();
    let age = age_threshold();
    sweep_rotated_logs(&dir, age);
    sweep_legacy_supervisor_log(&dir, age);
    sweep_orphan_bytelogs(live);
    sweep_binaries_prev();
    sweep_binaries_quarantine();
    trim_launchd_log(&dir.join("shelld.log"));
    trim_launchd_log(&dir.join("shelld.err"));
}

fn age_threshold() -> Duration {
    let days = std::env::var("MARSPOT_LOG_GC_AGE_D")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_AGE_DAYS);
    Duration::from_secs(days * 86400)
}

fn older_than(meta: &fs::Metadata, age: Duration) -> bool {
    meta.modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|d| d >= age)
        .unwrap_or(false)
}

fn sweep_rotated_logs(dir: &Path, age: Duration) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name();
        let s = name.to_string_lossy();
        if !s.starts_with("marspot.") {
            continue;
        }
        if s == "marspot.log" {
            continue; // active file — protected by name discrimination
        }
        if !(s.ends_with(".log") || s.ends_with(".log.gz")) {
            continue;
        }
        if let Ok(meta) = e.metadata() {
            if older_than(&meta, age) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

fn sweep_legacy_supervisor_log(dir: &Path, age: Duration) {
    let p = dir.join("supervisor.log");
    if let Ok(meta) = fs::metadata(&p) {
        if older_than(&meta, age) {
            let _ = fs::remove_file(&p);
        }
    }
}

fn sweep_orphan_bytelogs(live: &LiveSet) {
    sweep_orphan_bytelogs_at(&crate::paths::sessions_dir(), live);
}

/// Test hook for `sweep_orphan_bytelogs` — takes the sessions root
/// explicitly so a tempdir-based test doesn't depend on env state.
pub(crate) fn sweep_orphan_bytelogs_at(dir: &Path, live: &LiveSet) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let grace = Duration::from_secs(BYTELOG_GRACE_SECS);
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name();
        let s = name.to_string_lossy();
        let id: u64 = match s.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if live.session_ids.contains(&id) {
            continue;
        }
        // Use the bytelog's own mtime when it exists — the directory
        // mtime can be misleading after touch operations.
        let bytelog = e.path().join("bytelog");
        let cold = match fs::metadata(&bytelog) {
            Ok(m) => older_than(&m, grace),
            // No bytelog file at all — the dir is just an empty leftover.
            Err(_) => true,
        };
        if cold {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

fn sweep_binaries_prev() {
    let dir = crate::paths::binaries_root().join("prev");
    sweep_dir_by_age(&dir, Duration::from_secs(PREV_AGE_DAYS * 86400));
}

fn sweep_binaries_quarantine() {
    let dir = crate::paths::binaries_root().join("quarantine");
    sweep_dir_by_age(&dir, Duration::from_secs(QUARANTINE_AGE_DAYS * 86400));
}

fn sweep_dir_by_age(dir: &Path, age: Duration) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.filter_map(|e| e.ok()) {
        if let Ok(meta) = e.metadata() {
            if older_than(&meta, age) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// Tail-trim a launchd-managed log when it crosses the size cap. Never
/// unlinks — these files are panic + pre-init safety nets and have to
/// keep capturing whatever the next launchd respawn writes.
fn trim_launchd_log(path: &Path) {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.len() <= LAUNCHD_LOG_TRIM_BYTES {
        return;
    }
    let _ = trim_to_tail(path, LAUNCHD_LOG_RETAIN_BYTES);
}

fn trim_to_tail(path: &Path, retain_bytes: u64) -> std::io::Result<()> {
    let mut src = fs::File::open(path)?;
    let len = src.metadata()?.len();
    let keep_from = len.saturating_sub(retain_bytes);
    src.seek(SeekFrom::Start(keep_from))?;
    let mut tail = Vec::with_capacity(retain_bytes as usize);
    src.read_to_end(&mut tail)?;
    // Snap to the next newline so consumers don't open mid-line.
    let snap = tail
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let tail = &tail[snap..];
    let tmp = path.with_extension("trim-tmp");
    {
        let mut dst = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        dst.write_all(tail)?;
        dst.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-logx-gc-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn set_mtime_secs_ago(path: &Path, secs_ago: i64) {
        let cstr = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let target = now - secs_ago;
        let tvs = [
            libc::timeval { tv_sec: target as libc::time_t, tv_usec: 0 },
            libc::timeval { tv_sec: target as libc::time_t, tv_usec: 0 },
        ];
        let r = unsafe { libc::utimes(cstr.as_ptr(), tvs.as_ptr()) };
        assert_eq!(r, 0, "utimes failed");
    }

    #[test]
    fn sweep_rotated_unlinks_files_past_age_cap() {
        let dir = tmpdir();
        let old = dir.join("marspot.00000000000000000001.log.gz");
        let young = dir.join("marspot.00000000000000000002.log.gz");
        let active = dir.join("marspot.log");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&young, b"x").unwrap();
        std::fs::write(&active, b"x").unwrap();
        set_mtime_secs_ago(&old, 9 * 86400); // 9 days old
        set_mtime_secs_ago(&young, 60);
        set_mtime_secs_ago(&active, 9 * 86400); // even though "old", active is protected by name
        sweep_rotated_logs(&dir, Duration::from_secs(7 * 86400));
        assert!(!old.exists(), "9d rotated should be gone");
        assert!(young.exists(), "1m rotated should survive");
        assert!(active.exists(), "active is name-protected, never touched");
    }

    #[test]
    fn sweep_rotated_ignores_non_marspot_files() {
        let dir = tmpdir();
        let stranger = dir.join("not-ours.log.gz");
        std::fs::write(&stranger, b"x").unwrap();
        set_mtime_secs_ago(&stranger, 30 * 86400);
        sweep_rotated_logs(&dir, Duration::from_secs(7 * 86400));
        assert!(stranger.exists(), "GC must not touch foreign files");
    }

    #[test]
    fn orphan_bytelog_removed_when_id_not_live_and_cold() {
        let dir = tmpdir();
        // session 42 = live (kept), 99 = orphan + cold (removed),
        // 100 = orphan but recently touched (skipped via grace).
        for id in [42u64, 99, 100] {
            let sub = dir.join(id.to_string());
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("bytelog"), b"data").unwrap();
        }
        set_mtime_secs_ago(&dir.join("99/bytelog"), 7200); // 2h cold
        set_mtime_secs_ago(&dir.join("100/bytelog"), 60); // inside 1h grace
        let mut live = LiveSet::empty();
        live.session_ids.insert(42);
        sweep_orphan_bytelogs_at(&dir, &live);
        assert!(dir.join("42").exists(), "live session preserved");
        assert!(!dir.join("99").exists(), "cold orphan removed");
        assert!(dir.join("100").exists(), "recent orphan within grace preserved");
    }

    #[test]
    fn trim_launchd_log_keeps_tail_and_snaps_to_newline() {
        let dir = tmpdir();
        let p = dir.join("shelld.log");
        // Build a file > 16 MiB with one-line records so snap-to-newline
        // has something to align on.
        let mut payload = Vec::with_capacity(17 * 1024 * 1024);
        let mut i: u32 = 0;
        while payload.len() < 17 * 1024 * 1024 {
            payload.extend_from_slice(format!("line-{:08}\n", i).as_bytes());
            i += 1;
        }
        std::fs::write(&p, &payload).unwrap();
        trim_launchd_log(&p);
        let after = std::fs::read(&p).unwrap();
        // Should be roughly 1 MiB (the retain target), not the original 17 MiB.
        assert!(
            after.len() <= 2 * 1024 * 1024,
            "expected trim ≤2 MiB, got {}",
            after.len()
        );
        assert!(after.len() >= 512 * 1024, "expected trim ≥512 KiB, got {}", after.len());
        // First byte = post-newline (we snapped).
        assert_ne!(after[0], b'\n');
        // Last byte = newline (records are line-terminated).
        assert_eq!(*after.last().unwrap(), b'\n');
    }

    #[test]
    fn trim_launchd_log_skips_under_cap() {
        let dir = tmpdir();
        let p = dir.join("shelld.err");
        std::fs::write(&p, b"tiny\n").unwrap();
        trim_launchd_log(&p);
        assert_eq!(std::fs::read(&p).unwrap(), b"tiny\n");
    }
}
