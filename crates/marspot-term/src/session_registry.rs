//! RFC-003: on-disk session registry shared across L2 restarts.
//!
//! When L4 shelld goes away in Phase 6, L2 needs a way to track which
//! L3 sessions exist independently of any single L2 lifetime — silent
//! update swaps the L2 process, the new instance scans this registry
//! to reattach to the L3 children that survived.
//!
//! This module currently lands the **id allocator** only (Phase 2.1).
//! Phase 2.2 will add the per-session `.toml` files + scan API; Phase
//! 3 wires both into L2.
//!
//! Storage layout (under `MARSPOT_STATE_DIR/sessions/`):
//!
//!   .next_id            — monotonic counter, ASCII decimal u64
//!   <id>/bytelog        — per-session byte log (owned by L3 in
//!                          RFC-003)
//!   <id>.toml           — registry entry (Phase 2.2)
//!   <id>.sock           — L3 UDS control socket (Phase 2.3)

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::paths::sessions_dir;

/// File holding the next session id to allocate. Lives inside
/// `sessions_dir()` so it shares the directory's lifecycle.
fn next_id_path() -> PathBuf {
    sessions_dir().join(".next_id")
}

/// Atomically allocate the next session id.
///
/// Implementation: open `.next_id` r/w (create if absent), take an
/// exclusive `flock(LOCK_EX)`, read current value, increment, rewind +
/// rewrite, then release the lock by dropping the file handle.
///
/// flock is advisory but every allocator in marspot uses the same
/// function, so the lock binds the cohort. Concurrent allocators
/// serialise; a crashed allocator releases its lock when its file
/// handle goes away (Linux + macOS guarantee this on fd close).
///
/// First call on a fresh state dir returns 1; the counter never
/// rolls back even if a session is killed or its id falls out of use
/// (id reuse would race attach + spawn during silent update). u64
/// means the counter is effectively unlimited.
pub fn allocate_next_session_id() -> io::Result<u64> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)?;
    let path = next_id_path();

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;

    // SAFETY: file is a live fd; flock is the standard POSIX advisory
    // lock primitive. Released automatically when the fd closes.
    let fd = file.as_raw_fd();
    let r = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let current: u64 = buf.trim().parse().unwrap_or(0);
    let next = current + 1;

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    write!(file, "{next}")?;
    file.sync_all()?;

    // Lock released via Drop (file close).
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    // Tests in this module share one process-global env var
    // (MARSPOT_STATE_DIR), so cargo's parallel test scheduler would
    // race them. Serialise via a static Mutex — single suite, no extra
    // crate dependency needed.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct StateDirGuard {
        prev: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: std::path::PathBuf,
    }
    impl StateDirGuard {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().expect("env lock");
            let prev = std::env::var_os("MARSPOT_STATE_DIR");
            // Unique per-test sandbox under the system temp dir.
            let stamp = format!(
                "rfc003-registry-{}-{:p}",
                std::process::id(),
                &lock as *const _
            );
            let dir = std::env::temp_dir().join(stamp);
            // Best-effort cleanup of prior debris from the same path.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create sandbox dir");
            std::env::set_var("MARSPOT_STATE_DIR", &dir);
            Self { prev, _lock: lock, _dir: dir }
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
            match self.prev.take() {
                Some(v) => std::env::set_var("MARSPOT_STATE_DIR", v),
                None => std::env::remove_var("MARSPOT_STATE_DIR"),
            }
        }
    }

    /// Each call to `allocate_next_session_id` returns a fresh,
    /// monotonically increasing value starting at 1.
    #[test]
    fn allocates_monotonic_starting_at_one() {
        let _g = StateDirGuard::new();
        for expect in 1u64..=5 {
            let got = allocate_next_session_id().expect("allocate");
            assert_eq!(got, expect, "expected next_id={expect}, got={got}");
        }
    }

    /// Two threads racing on the same registry get distinct ids — the
    /// `flock` serialises them so we never alias.
    #[test]
    fn concurrent_allocation_yields_distinct_ids() {
        let _g = StateDirGuard::new();
        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b.wait();
                allocate_next_session_id().expect("allocate")
            }));
        }
        let mut ids: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort();
        assert_eq!(ids.len(), N);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, (i + 1) as u64, "got ids {ids:?}");
        }
    }
}
