//! Cold-data garbage collection.
//!
//! `sweep_startup()` — every binary calls this once at the end of
//! `logx::init()` in a detached thread. It only touches the shared log
//! directory's rotated structured logs; cheap and idempotent.
//!
//! The active `marspot.log` is NEVER touched: the rotated-file glob
//! requires a `marspot.<digits>.` prefix, and the active file is plain
//! `marspot.log`.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::sink::log_dir;

const DEFAULT_AGE_DAYS: u64 = 7;

pub fn sweep_startup() {
    let dir = log_dir();
    let age = age_threshold();
    sweep_rotated_logs(&dir, age);
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
        if let Ok(meta) = e.metadata()
            && older_than(&meta, age)
        {
            let _ = fs::remove_file(e.path());
        }
    }
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
            libc::timeval {
                tv_sec: target as libc::time_t,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: target as libc::time_t,
                tv_usec: 0,
            },
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
}
