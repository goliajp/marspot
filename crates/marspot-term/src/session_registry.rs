//! RFC-003: on-disk session registry shared across L2 restarts.
//!
//! When L4 shelld goes away in Phase 6, L2 needs a way to track which
//! L3 sessions exist independently of any single L2 lifetime — silent
//! update swaps the L2 process, the new instance scans this registry
//! to reattach to the L3 children that survived.
//!
//! Lands in two parts: 2a is the id allocator, 2b adds the per-session
//! `entry.toml` file + scan / read / delete API.  Phase 3 wires this
//! into L2.
//!
//! Storage layout (under `MARSPOT_STATE_DIR/sessions/`):
//!
//!   .next_id            — monotonic counter, ASCII decimal u64
//!   <id>/bytelog        — per-session byte log (since 1a)
//!   <id>/entry.toml     — registry entry (this commit, 2b)
//!   <id>/sock           — L3 UDS control socket (Phase 2.3 / 2c)

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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
///
/// RFC-004 A.1 self-heal: the counter value is floored at
/// `max(existing session dir ids)` on every allocation.  A deleted /
/// truncated / corrupt `.next_id` used to reset the counter to 0 and
/// hand out ids that COLLIDE with surviving `sessions/<id>/` dirs —
/// two L3s then share one dir with no lock and interleave writes
/// into the same scrollback.bin (the session-347 corruption class).
/// The dir scan is O(#sessions) inside the flock and only runs on
/// allocation (pane spawn), never on a hot path.
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
    let counter: u64 = buf.trim().parse().unwrap_or(0);
    let dir_floor = max_existing_session_id(&dir);
    let next = counter.max(dir_floor) + 1;

    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    write!(file, "{next}")?;
    file.sync_all()?;

    // Lock released via Drop (file close).
    Ok(next)
}

/// RFC-004 A.3 — per-session-dir owner lock.  Exactly one L3 process
/// may own `sessions/<id>/` at a time; ownership is an exclusive
/// non-blocking `flock` on `sessions/<id>/.lock`, held for the
/// process's whole life (the fd is deliberately leaked into the
/// process — flock releases on last close, which includes process
/// death and survives execv when CLOEXEC is cleared).
///
/// Before this lock, mutual exclusion was pure convention ("L2 only
/// spawns dead ids") — a second L3 landing on the same id would
/// unlink the first's socket and overwrite entry.toml, and both
/// would interleave appends into the same unlocked scrollback.bin
/// (the session-347 corruption class).
///
/// Returns the held lock file on success; `WouldBlock` when another
/// process owns the dir.
pub fn try_lock_session_dir(id: u64) -> io::Result<std::fs::File> {
    let dir = session_dir(id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(".lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// RFC-004 A.2 — identity-verified liveness.  `kill(pid, 0)` alone is
/// NOT a valid "this session's L3 is alive" test: after a reboot (or
/// any long gap) the recorded pid may have been recycled by an
/// arbitrary unrelated process.  Treating a recycled pid as a live L3
/// used to send it SIGKILL on reattach failure — killing an innocent
/// process — and then `delete_session` destroyed the session's
/// history.  A live *session* requires BOTH:
///
///   1. `kill(pid, 0)` succeeds (a process exists and is signalable), and
///   2. `proc_pidpath(pid)` resolves to an executable whose file name
///      contains "marspot-session".
///
/// proc_pidpath failing (process died between the two calls, or a
/// sandbox denies the query for a foreign process — which by itself
/// proves the pid is not our child) counts as NOT a session.  Never
/// signal a pid that fails this check.
pub fn pid_is_live_session(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe {
        libc::proc_pidpath(
            pid,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return false;
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]);
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.contains("marspot-session"))
}

/// Highest numeric session-dir id currently on disk, 0 when none.
/// Used as the allocation floor so a lost `.next_id` can never
/// hand out an id that collides with a surviving session dir.
fn max_existing_session_id(sessions_root: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(sessions_root) else {
        return 0;
    };
    let mut max = 0u64;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !is_session_dir(&path) {
            continue;
        }
        if let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<u64>().ok())
        {
            max = max.max(id);
        }
    }
    max
}

// ──────────────────────────────────────────────────────────────
// Per-session entry — Phase 2b
// ──────────────────────────────────────────────────────────────

/// Re-export the L2↔L3 wire protocol version so callers writing
/// entry.toml mark it with the same number their Hello handshake
/// will agree on.  Lives in `shell_proto` since that's the wire
/// definition; this re-export keeps the registry self-contained.
pub use crate::shell_proto::PROTO_VERSION;

/// Per-session directory: `sessions/<id>/`.
pub fn session_dir(id: u64) -> PathBuf {
    sessions_dir().join(id.to_string())
}

/// Where the registry entry lives.
pub fn session_entry_path(id: u64) -> PathBuf {
    session_dir(id).join("entry.toml")
}

/// Where the L3 UDS listener binds.
pub fn session_socket_path(id: u64) -> PathBuf {
    session_dir(id).join("sock")
}

/// Persistent scrollback data file (A1 of pane upgrade — see
/// `docs/scrollback-search.md`).  Append-only Cell records prefixed
/// with `rec_len` for crash-safe trailing-record trim.
pub fn scrollback_bin_path(id: u64) -> PathBuf {
    session_dir(id).join("scrollback.bin")
}

/// Sidecar index for `scrollback_bin_path` — dense `[u64 LE
/// byte_offset]` array with an EOF sentinel so `len(idx) - 1` =
/// scrollback line count.  Rebuildable from `.bin` if missing /
/// truncated.
pub fn scrollback_idx_path(id: u64) -> PathBuf {
    session_dir(id).join("scrollback.idx")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: u64,
    pub pid: i32,
    pub socket: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub cwd: String,
    pub proto_version: u32,
    pub created_at_unix: u64,
    /// RFC-003 Amendment 7: shm region name the L3 publishes into.
    /// A freshly-spawned L2 after a silent-update swap can shm_open
    /// this name to reattach to a surviving L3's framebuffer (the fd
    /// doesn't survive the swap; the name does).  Empty string =
    /// legacy entry written before this field existed.
    pub shm_name: String,
    /// RFC-003 §6 cc: the L3's forkpty child PID (the zsh / login
    /// shell at the other end of the PTY).  L1-side plugins
    /// (claudecode, future ones) walk pidtree from this pid to find
    /// descendants like `claude` they want to badge / send input to.
    /// `0` = legacy entry written before this field existed.
    pub shell_child_pid: i32,
}

fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn unquote(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
}

/// Write the entry atomically: serialize → tempfile + rename. On any
/// failure the previous entry (or no entry) is what stays on disk.
pub fn write_session_entry(entry: &SessionEntry) -> io::Result<()> {
    let dir = session_dir(entry.id);
    std::fs::create_dir_all(&dir)?;
    let path = session_entry_path(entry.id);
    let tmp = path.with_extension("toml.tmp");
    let mut s = String::new();
    use std::fmt::Write;
    writeln!(s, "# marspot session registry entry — RFC-003").ok();
    writeln!(s, "id = {}", entry.id).ok();
    writeln!(s, "pid = {}", entry.pid).ok();
    writeln!(s, "socket = {}", quote(&entry.socket.to_string_lossy())).ok();
    writeln!(s, "cols = {}", entry.cols).ok();
    writeln!(s, "rows = {}", entry.rows).ok();
    writeln!(s, "title = {}", quote(&entry.title)).ok();
    writeln!(s, "cwd = {}", quote(&entry.cwd)).ok();
    writeln!(s, "proto_version = {}", entry.proto_version).ok();
    writeln!(s, "created_at_unix = {}", entry.created_at_unix).ok();
    writeln!(s, "shm_name = {}", quote(&entry.shm_name)).ok();
    writeln!(s, "shell_child_pid = {}", entry.shell_child_pid).ok();
    std::fs::write(&tmp, s)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn invalid<T: Into<String>>(msg: T) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Parse one entry.toml. Tolerates unknown fields and blank/comment
/// lines; rejects only when a required field is missing or the value
/// can't be parsed at its declared type.
pub fn parse_session_entry_text(contents: &str) -> io::Result<SessionEntry> {
    use std::collections::HashMap;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        fields.insert(key, val);
    }
    let req_str = |k: &str| -> io::Result<String> {
        let raw = fields.get(k).ok_or_else(|| invalid(format!("missing {k}")))?;
        unquote(raw).ok_or_else(|| invalid(format!("{k} not quoted")))
    };
    let req_num = |k: &str| -> io::Result<u64> {
        let raw = fields.get(k).ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u64>().map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_i32 = |k: &str| -> io::Result<i32> {
        let raw = fields.get(k).ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<i32>().map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_u16 = |k: &str| -> io::Result<u16> {
        let raw = fields.get(k).ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u16>().map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    let req_u32 = |k: &str| -> io::Result<u32> {
        let raw = fields.get(k).ok_or_else(|| invalid(format!("missing {k}")))?;
        raw.parse::<u32>().map_err(|e| invalid(format!("bad {k}: {e}")))
    };
    // Optional field for back-compat with entries written before
    // Amendment 7 added shm_name.  Missing / unquoted = "".
    let opt_str = |k: &str| -> String {
        fields
            .get(k)
            .and_then(|raw| unquote(raw))
            .unwrap_or_default()
    };
    let opt_i32 = |k: &str| -> i32 {
        fields
            .get(k)
            .and_then(|raw| raw.parse::<i32>().ok())
            .unwrap_or(0)
    };
    Ok(SessionEntry {
        id: req_num("id")?,
        pid: req_i32("pid")?,
        socket: PathBuf::from(req_str("socket")?),
        cols: req_u16("cols")?,
        rows: req_u16("rows")?,
        title: req_str("title")?,
        cwd: req_str("cwd")?,
        proto_version: req_u32("proto_version")?,
        created_at_unix: req_num("created_at_unix")?,
        shm_name: opt_str("shm_name"),
        shell_child_pid: opt_i32("shell_child_pid"),
    })
}

pub fn read_session_entry(id: u64) -> io::Result<SessionEntry> {
    let contents = std::fs::read_to_string(session_entry_path(id))?;
    parse_session_entry_text(&contents)
}

/// Remove this session's whole directory tree (entry.toml + sock +
/// bytelog). Called when L2 has confirmed the L3 is dead and the
/// session id is being retired.
pub fn delete_session(id: u64) -> io::Result<()> {
    let dir = session_dir(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Best-effort unlink of a leftover socket file at a session's UDS
/// path. L3 calls this on boot so its `bind(2)` doesn't trip over a
/// stale path left behind by a prior process that died before
/// `delete_session`.
pub fn cleanup_stale_socket(id: u64) {
    let p = session_socket_path(id);
    let _ = std::fs::remove_file(&p);
}

/// Scan `sessions/` and return every entry that parses successfully.
/// Stale entries that fail to parse (corrupt write, wrong format) are
/// skipped — `list_session_entries` is for discovery, callers do
/// liveness validation (`kill 0`) themselves.
pub fn list_session_entries() -> Vec<SessionEntry> {
    let root = sessions_dir();
    let read_dir = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !is_session_dir(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Ok(id) = name.parse::<u64>() else { continue };
        if let Ok(e) = read_session_entry(id) {
            out.push(e);
        }
    }
    out
}

fn is_session_dir(p: &Path) -> bool {
    p.is_dir()
        && p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
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
            // SAFETY: test/example code, single-threaded at this point (state-dir
            // mutations additionally serialized by the suite's state-dir lock).
            unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
            Self { prev, _lock: lock, _dir: dir }
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
            match self.prev.take() {
                // SAFETY: test/example code, single-threaded at this point (state-dir
                // mutations additionally serialized by the suite's state-dir lock).
                Some(v) => unsafe { std::env::set_var("MARSPOT_STATE_DIR", v) },
                // SAFETY: test/example code, single-threaded at this point (state-dir
                // mutations additionally serialized by the suite's state-dir lock).
                None => unsafe { std::env::remove_var("MARSPOT_STATE_DIR") },
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

    /// RFC-004 A.1 — a lost/corrupt `.next_id` must not hand out ids
    /// that collide with surviving session dirs: allocation is floored
    /// at max(existing dir id).
    #[test]
    fn allocation_self_heals_from_lost_counter() {
        let _g = StateDirGuard::new();
        // Simulate surviving session dirs 5 and 12 with NO .next_id
        // (the crash-lost-counter scenario).
        write_session_entry(&sample_entry(5)).expect("write 5");
        write_session_entry(&sample_entry(12)).expect("write 12");
        assert!(!next_id_path().exists(), "counter must start absent");
        let got = allocate_next_session_id().expect("allocate");
        assert_eq!(got, 13, "must floor at max existing dir id (12) + 1");
        // Corrupt the counter backwards; next allocation still can't
        // collide with dir 13's... (13 has no dir yet) — recreate the
        // regression shape: counter says 2, dirs go up to 13.
        write_session_entry(&sample_entry(13)).expect("write 13");
        std::fs::write(next_id_path(), "2").expect("corrupt counter");
        let got = allocate_next_session_id().expect("allocate");
        assert_eq!(got, 14, "corrupt low counter must not re-issue 3");
    }

    /// RFC-004 A.3 — session-dir lock is exclusive per id, released
    /// on drop, and independent across ids.
    #[test]
    fn session_dir_lock_is_exclusive_and_releases_on_drop() {
        let _g = StateDirGuard::new();
        let first = try_lock_session_dir(21).expect("first lock");
        // Same id: second attempt must fail while the first is held.
        let second = try_lock_session_dir(21);
        assert!(
            second.is_err(),
            "second lock on same id must be refused: {second:?}"
        );
        // Different id: independent.
        let _other = try_lock_session_dir(22).expect("independent id");
        // Release → re-acquire succeeds.
        drop(first);
        try_lock_session_dir(21).expect("relock after release");
    }

    /// RFC-004 A.2 — identity-verified liveness: our own pid is alive
    /// but is NOT a marspot-session image; pid 1 (launchd) is alive
    /// and foreign; a wildly-invalid pid is dead.
    #[test]
    fn pid_liveness_requires_session_identity() {
        assert!(
            !pid_is_live_session(std::process::id() as i32),
            "test binary is not marspot-session"
        );
        assert!(!pid_is_live_session(1), "launchd is not marspot-session");
        assert!(!pid_is_live_session(0));
        assert!(!pid_is_live_session(-1));
        assert!(!pid_is_live_session(i32::MAX), "unallocated pid is dead");
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

    fn sample_entry(id: u64) -> SessionEntry {
        SessionEntry {
            id,
            pid: 12345 + id as i32,
            socket: session_socket_path(id),
            cols: 120,
            rows: 40,
            title: "zsh".into(),
            cwd: "/Users/test/with spaces and \"quotes\"".into(),
            proto_version: PROTO_VERSION,
            created_at_unix: 1_718_000_000 + id,
            shm_name: format!("/msp-s-{id}"),
            shell_child_pid: 99000 + id as i32,
        }
    }

    #[test]
    fn entry_roundtrip_persists_all_fields() {
        let _g = StateDirGuard::new();
        let want = sample_entry(7);
        write_session_entry(&want).expect("write");
        let got = read_session_entry(7).expect("read");
        assert_eq!(got, want, "roundtrip mismatch");
        assert!(session_entry_path(7).exists());
    }

    #[test]
    fn delete_session_removes_everything() {
        let _g = StateDirGuard::new();
        let e = sample_entry(3);
        write_session_entry(&e).expect("write");
        // Drop a fake socket file too, mimic a live session.
        std::fs::write(session_socket_path(3), "").expect("touch sock");
        assert!(session_dir(3).exists());

        delete_session(3).expect("delete");

        assert!(!session_dir(3).exists(), "session dir should be gone");
        // Idempotent — second delete returns Ok.
        delete_session(3).expect("delete again");
    }

    #[test]
    fn list_session_entries_skips_non_numeric_and_corrupt() {
        let _g = StateDirGuard::new();
        write_session_entry(&sample_entry(1)).expect("write 1");
        write_session_entry(&sample_entry(5)).expect("write 5");

        // A non-numeric subdir is ignored.
        std::fs::create_dir_all(sessions_dir().join("notanumber")).expect("mkdir noise");

        // A numeric subdir whose entry.toml is garbage is skipped.
        std::fs::create_dir_all(sessions_dir().join("9")).expect("mkdir 9");
        std::fs::write(sessions_dir().join("9/entry.toml"), "garbage\n").expect("write garbage");

        let mut ids: Vec<u64> = list_session_entries().iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 5], "expected only {{1, 5}}, got {ids:?}");
    }

    #[test]
    fn cleanup_stale_socket_is_idempotent_and_unlinks_only_that_id() {
        let _g = StateDirGuard::new();
        write_session_entry(&sample_entry(2)).expect("write");
        std::fs::write(session_socket_path(2), "").expect("touch sock");
        assert!(session_socket_path(2).exists());

        cleanup_stale_socket(2);
        assert!(!session_socket_path(2).exists(), "sock should be gone");

        // Second call on already-gone path is fine.
        cleanup_stale_socket(2);
        // entry.toml + dir still intact — cleanup is narrowly scoped.
        assert!(session_entry_path(2).exists(), "entry.toml must survive");
    }

    #[test]
    fn parse_rejects_missing_required_field() {
        let txt = "id = 1\npid = 2\ncols = 80\n"; // missing rows / socket / etc
        let err = parse_session_entry_text(txt).expect_err("should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
