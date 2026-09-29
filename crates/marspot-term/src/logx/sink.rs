//! Single-file shared sink for the marspot structured-log stream.
//!
//! All five components (shelld, shell, core, session, gui) open their
//! own O_APPEND fd onto the same `paths::log_dir()/marspot.log`. POSIX
//! guarantees write-syscall atomicity for `< PIPE_BUF` (512 B on macOS)
//! to an O_APPEND fd; per-process `Mutex<Sink>` serialises intra-process
//! writes so the on-disk bytes mirror the logical event stream exactly.
//! Cross-process rotation coordination lives in `super::rotate`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 8 MiB default rotate-on-size cap. Override via `MARSPOT_LOG_MAX_MB`.
pub const DEFAULT_MAX_MB: u64 = 8;
/// 24 h rotate-on-idle. Keeps the rotated-filename timestamp a useful
/// index even on daemons that hardly log anything.
pub const ROTATE_AGE: Duration = Duration::from_secs(24 * 3600);
/// Re-stat the active path every N writes to detect cross-process
/// rotations under us. Lower → faster detection but more syscalls;
/// 256 keeps fstat to ~0.1 % of writes.
const STAT_INTERVAL: u32 = 256;

pub(crate) static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

pub(crate) struct Sink {
    pub dir: PathBuf,
    pub path: PathBuf,
    pub file: File,
    pub inode: u64,
    pub bytes: u64,
    pub rotated_at: Instant,
    pub writes_since_stat: u32,
    pub max_bytes: u64,
}

impl Sink {
    pub fn open() -> std::io::Result<Self> {
        Self::open_at_dir(log_dir(), env_max_bytes())
    }

    /// Test hook: instantiate a sink rooted at an arbitrary directory
    /// with a caller-chosen size cap. The rotate path reads `dir` off
    /// `Self` (not env), so tests don't have to fight `OnceLock` global
    /// state or env-var staleness across cases.
    pub fn open_at_dir(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("marspot.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let meta = file.metadata()?;
        Ok(Self {
            dir,
            path,
            file,
            inode: meta.ino(),
            bytes: meta.len(),
            rotated_at: Instant::now(),
            writes_since_stat: 0,
            max_bytes,
        })
    }

    /// Close the current fd and reopen `path`. Used when a sibling
    /// process rotated the active file out from under us — our fd now
    /// points at a renamed inode and would write into the rotated
    /// file instead of the fresh active one — and when the file was
    /// removed entirely.
    ///
    /// The directory is re-created first, because the removal is not
    /// always ours to explain: on 2026-07-31 a third-party cleaner
    /// (Tencent Lemon) deleted `~/Library/Logs/Marspot` wholesale while
    /// marspot was running. The unlink case was already handled, but
    /// `open` with a missing parent fails, the error was swallowed, and
    /// the process kept appending to the now-unreachable inode: logs
    /// silently stopped for the rest of that process's life, which on a
    /// terminal meant for weeks of uptime is the whole log.
    pub fn reopen(&mut self) -> std::io::Result<()> {
        let _ = std::fs::create_dir_all(&self.dir);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let meta = file.metadata()?;
        self.file = file;
        self.inode = meta.ino();
        self.bytes = meta.len();
        self.writes_since_stat = 0;
        self.rotated_at = Instant::now();
        Ok(())
    }
}

/// Initialise the singleton sink. Failure to open the real path falls
/// back to a degenerate sink targeting `/dev/null` so logging never
/// crashes the calling process — this is logging, not the hot path.
pub fn ensure_open() -> std::io::Result<()> {
    SINK.get_or_init(|| {
        Mutex::new(match Sink::open() {
            Ok(s) => s,
            Err(_) => Sink {
                dir: PathBuf::from("/dev"),
                path: PathBuf::from("/dev/null"),
                file: OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("open /dev/null"),
                inode: 0,
                bytes: 0,
                rotated_at: Instant::now(),
                writes_since_stat: 0,
                max_bytes: u64::MAX,
            },
        })
    });
    Ok(())
}

/// The one shared write path. Locks the sink, writes the pre-assembled
/// line in a single syscall, and triggers `rotate::check_and_maybe_rotate`
/// when the size estimate or write count crosses the threshold.
pub fn write_line(line: &[u8]) {
    let Some(mtx) = SINK.get() else {
        return;
    };
    let mut sink = match mtx.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    if sink.file.write_all(line).is_err() {
        // Disk full, fd closed under us, etc. Drop silently — log
        // failures never escalate to caller failures.
        return;
    }
    sink.bytes += line.len() as u64;
    sink.writes_since_stat += 1;
    let should_check = sink.writes_since_stat >= STAT_INTERVAL || sink.bytes >= sink.max_bytes;
    if !should_check {
        return;
    }
    sink.writes_since_stat = 0;
    let _ = super::rotate::check_and_maybe_rotate(&mut sink);
}

pub(crate) fn log_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MARSPOT_LOG_DIR") {
        return PathBuf::from(dir);
    }
    crate::paths::log_dir()
}

fn env_max_bytes() -> u64 {
    std::env::var("MARSPOT_LOG_MAX_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_MB)
        .saturating_mul(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// A third-party cleaner removing the log directory must not
    /// silence logging for the rest of the process's life.
    ///
    /// This is not hypothetical: on 2026-07-31 Tencent Lemon deleted
    /// `~/Library/Logs/Marspot` while marspot was running, and the
    /// process kept appending to the unlinked inode — `lsof` showed the
    /// fd still pointing at a path that no longer existed, and nothing
    /// new was ever written where anyone could read it.  The unlink
    /// case was handled; the missing-parent case was not.
    #[test]
    fn writing_after_the_log_directory_is_deleted_recreates_it() {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("marspot-logx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: nextest gives each test its own process, and the sink
        // singleton below is initialised from this value.
        unsafe { std::env::set_var("MARSPOT_LOG_DIR", &dir) };

        crate::logx::init("test");
        crate::lx_info!("logx.test", "before the cleaner");
        let path = dir.join("marspot.log");
        assert!(path.exists(), "log file should exist to begin with");

        // The cleaner takes the whole directory, not just the file.
        std::fs::remove_dir_all(&dir).expect("remove the log dir");
        assert!(!path.exists());

        // Enough writes to reach the periodic stat that notices.
        for i in 0..(super::STAT_INTERVAL + 8) {
            crate::lx_info!("logx.test", "after the cleaner", i = i as u64);
        }

        assert!(
            path.exists(),
            "the sink should have re-created its directory"
        );
        let body = std::fs::read_to_string(&path).expect("read the new log");
        assert!(
            body.contains("after the cleaner"),
            "the new file should hold the lines written after the deletion"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
