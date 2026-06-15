//! marspot-shelld — per-user daemon that owns shell PTYs.
//!
//! Run by launchd via `~/Library/LaunchAgents/com.marspot.shelld.plist`
//! (KeepAlive=true).  marspot GUI talks to it over a unix socket at
//! `~/Library/Caches/marspot/shelld.sock`.
//!
//! Why split it out: the GUI process can die and respawn (silent
//! update, crash, manual restart) without taking the user's shell
//! processes with it.  The shell's `getppid()` is launchd, not the
//! GUI; SIGHUP on GUI exit doesn't propagate.
//!
//! Phase 3 scope: real session management.  NewSession forks a zsh
//! via the lib's `Pty`; Attach/Detach add/remove client subscribers;
//! Data frames stream PTY output to every attached client; Input
//! writes back to the master.  Bytelog (replay-on-attach) lands in
//! Phase 5.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write as IoWrite};
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use marspot::pty::{Pty, PtyConfig, TerminalSize};
use marspot::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot::shelld_proto::{
    decode_data, decode_hello, decode_new_session, decode_resize, decode_session_id, encode_data,
    encode_error, encode_hello_ack, encode_list_sessions_reply, encode_new_session_reply, Frame,
    MsgType, SessionInfo, PROTO_VERSION,
};

/// Per-session byte log cap.  When the log file grows past this we
/// compact: keep the most recent `BYTELOG_RETAIN_BYTES` of bytes,
/// drop the rest.  100 MiB is generous — even a `cat /dev/urandom`
/// run for several seconds doesn't fill it.
const BYTELOG_CAP_BYTES: u64 = 100 * 1024 * 1024;
const BYTELOG_RETAIN_BYTES: u64 = 50 * 1024 * 1024;
/// Send replay in chunks so a multi-MB log doesn't land as one
/// gigantic frame.  Each chunk fits comfortably inside
/// `MAX_PAYLOAD_LEN` (8 MiB) and below the OS socket buffer so the
/// writer thread can serialise the frame fast.
const REPLAY_CHUNK_BYTES: usize = 64 * 1024;

/// Process-wide shutdown flag.  Set by SIGTERM/SIGINT handler; observed
/// by per-client handlers (the accept loop is woken separately by
/// closing `LISTENER_FD` below — `std`'s accept silently retries
/// EINTR, so a signal alone wouldn't unstick it).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Last terminating signal observed.  Recorded by the SIGTERM/SIGINT
/// handler so the main loop, on noticing SHUTDOWN, can log *which*
/// signal triggered the exit — the signal handler itself is
/// async-signal-safe (no allocation, no log infra), so this is the
/// only sanctioned way to attribute a shelld shutdown to its cause.
/// Without this, marspot.log shows `SHELLD_STOP` with no provenance
/// (the cascade that killed 9 claudecode sessions on 2026-06-15 was
/// only diagnosed by cross-referencing stderr).  `0` = not set.
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);
/// Raw fd of the listening socket, published after `bind` so the
/// signal handler can `close(2)` it (AS-safe per POSIX) and force
/// the in-flight `accept` to return EBADF.  Negative sentinel before
/// init or after close.
static LISTENER_FD: AtomicI32 = AtomicI32::new(-1);
/// Set by the SIGUSR1 handler.  Polled by the accept loop, which —
/// when it sees the flag set — runs `do_execv_swap` to promote
/// `binaries/pending/marspot-shelld` into `current/`, serialise every
/// session's (master fd, child pid) into a handoff manifest, clear
/// CLOEXEC on the listen + master fds, and `execv` over its own image.
/// PID is preserved across `execv`, so the children are still our
/// children in the new image and the master fds (kernel-side) outlive
/// the swap.  Sessions survive; only the GUI/L2 client connections
/// drop and reattach + bytelog-replay on the next tick.
static EXEC_TRIGGER: AtomicBool = AtomicBool::new(false);
/// Write end of a self-pipe.  The SIGUSR1 handler scribbles a byte
/// here to wake the accept loop out of its `poll`; the read end is
/// part of the poll set.  Marked CLOEXEC so it doesn't leak across
/// the eventual execv (new image creates its own pair).
static EXEC_WAKE_FD: AtomicI32 = AtomicI32::new(-1);
/// Monotonic session-id allocator.  Never reused (even after a
/// session ends) so a stale client reference can't accidentally
/// land on a brand-new session.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// Monotonic subscriber-id allocator.  Each client connection that
/// attaches gets a fresh id so the session's subscriber Vec can
/// remove the exact entry on detach without relying on
/// SyncSender identity (which isn't exposed in std).
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);
/// Bounded per-subscriber send queue.  Backpressure: when a client
/// can't keep up, shelld's writer thread blocks on `send`, which
/// stalls the per-session broadcaster, which stalls the PTY reader
/// — exactly mirroring how the kernel pipe would backpressure the
/// child process today.
const SUBSCRIBER_QUEUE_DEPTH: usize = 64;

/// Always-on socket path.  Created on startup, removed on graceful
/// shutdown.  Resolved through `marspot::paths` so a dev / test
/// sandbox (`MARSPOT_STATE_DIR=…`) gets its own daemon on its own
/// socket and never collides with the installed LaunchAgent
/// instance.
fn socket_path() -> PathBuf {
    marspot::paths::shelld_socket()
}

/// One live shell session.  Owned by `Sessions` via `Arc`; subscribers
/// hold a `Weak` so a session dropping out from under them is a
/// recoverable error rather than a use-after-free.
struct Subscriber {
    id: u64,
    tx: SyncSender<Frame>,
}

/// Disk-backed append log of raw PTY bytes per session.  Used to
/// replay state when a client (re)attaches: the entire current log
/// is streamed as DATA frames before the live stream resumes.
///
/// On disk: one file per session at
/// `~/Library/Caches/marspot/sessions/<id>/bytelog`.  Capped at
/// `BYTELOG_CAP_BYTES`; on overflow we copy the last
/// `BYTELOG_RETAIN_BYTES` to a fresh file and replace the old one
/// (a few-ms compaction triggered at most once per ~50 MiB write
/// burst).  Survives shelld restarts so reattach from a fresh
/// daemon also gets prior history.
struct ByteLog {
    path: PathBuf,
    file: File,
    bytes_written: u64,
}

impl ByteLog {
    fn open(session_id: u64) -> io::Result<Self> {
        let dir = marspot::paths::sessions_dir().join(session_id.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("bytelog");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            bytes_written,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.bytes_written += bytes.len() as u64;
        if self.bytes_written > BYTELOG_CAP_BYTES {
            // Best-effort compaction: failure here just leaves the
            // log oversized until the next append tries again.
            let _ = self.compact();
        }
        Ok(())
    }

    fn compact(&mut self) -> io::Result<()> {
        let tmp_path = self.path.with_extension("tmp");
        {
            let mut src = OpenOptions::new().read(true).open(&self.path)?;
            let len = src.metadata()?.len();
            let keep_from = len.saturating_sub(BYTELOG_RETAIN_BYTES);
            src.seek(SeekFrom::Start(keep_from))?;
            let mut dst = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp_path)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n])?;
            }
        }
        std::fs::rename(&tmp_path, &self.path)?;
        // Reopen the file handle so append picks up the truncated
        // state (the previous fd points at the old inode).
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        self.bytes_written = self.file.metadata()?.len();
        Ok(())
    }

    /// Stream current contents to `out` in REPLAY_CHUNK_BYTES-sized
    /// DATA frames.  Called on ATTACH so the client can rebuild the
    /// terminal state.  Reads from the start of the file; the file
    /// is open in append mode so the read fd's position is
    /// independent of where writes append.
    fn replay(&self, session_id: u64, out: &SyncSender<Frame>) -> io::Result<()> {
        let mut f = OpenOptions::new().read(true).open(&self.path)?;
        f.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; REPLAY_CHUNK_BYTES];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let frame = Frame::new(MsgType::Data, encode_data(session_id, &buf[..n]));
            if out.send(frame).is_err() {
                // Client gone — abort replay quietly.
                return Ok(());
            }
        }
        // Sentinel: a zero-length DATA frame tells the client
        // "replay done, next DATA is live".  The client ignores
        // empty DATA for normal flow so this is benign even when
        // attach didn't request a replay.
        let _ = out.send(Frame::new(MsgType::Data, encode_data(session_id, &[])));
        Ok(())
    }

}

/// Helper for KILL_SESSION cleanup — removes the bytelog file and
/// its session directory.  Called from the Kill arm so a session
/// killed by the GUI doesn't leave gigabytes of log behind.
fn delete_bytelog(session_id: u64) {
    let dir = marspot::paths::sessions_dir().join(session_id.to_string());
    let _ = std::fs::remove_dir_all(&dir);
}

struct ShellSession {
    id: u64,
    pty: Arc<Pty>,
    /// All attached clients' inbound queues.  PTY reader thread
    /// fan-outs each chunk here.  Mutex hold is brief (clone of
    /// senders + drop dead ones).
    subscribers: Mutex<Vec<Subscriber>>,
    /// `false` once the child has exited; LIST_SESSIONS exposes
    /// this so the GUI can render an "exited" indicator.  Set by
    /// the reader thread on EOF.
    alive: AtomicBool,
    /// Append log of every byte read from the PTY.  Mutex-guarded
    /// because the reader thread is sole writer but `replay()` from
    /// ATTACH may run concurrently and needs the path to match what
    /// it was when the reader last appended.
    bytelog: Mutex<Option<ByteLog>>,
    /// User-set display title.  Empty string means "no custom
    /// title; show the default".  Persists across core restarts /
    /// dual-core swaps because shelld outlives core.  Set via the
    /// SET_TITLE control frame from core's `commit_title_edit`;
    /// returned in LIST_SESSIONS_REPLY so a freshly-spawned core
    /// repopulates its custom-title map on boot.
    title: Mutex<String>,
}

impl ShellSession {
    fn broadcast(&self, frame: Frame) {
        // Snapshot (id, sender) pairs under lock so the broadcast
        // loop itself doesn't hold the lock while individual sends
        // block.
        let snapshot: Vec<(u64, SyncSender<Frame>)> = {
            let g = self.subscribers.lock().unwrap();
            g.iter().map(|s| (s.id, s.tx.clone())).collect()
        };
        let mut dead: Vec<u64> = Vec::new();
        for (id, sender) in &snapshot {
            if sender.send(frame.clone()).is_err() {
                dead.push(*id);
            }
        }
        if !dead.is_empty() {
            let mut g = self.subscribers.lock().unwrap();
            g.retain(|s| !dead.contains(&s.id));
        }
    }

    /// Add a subscriber.  Returns its id so the caller can detach
    /// later.  The returned id is the only handle that addresses
    /// this exact subscription.
    fn attach(&self, tx: SyncSender<Frame>) -> u64 {
        let id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::AcqRel);
        self.subscribers.lock().unwrap().push(Subscriber { id, tx });
        id
    }

    fn detach(&self, subscriber_id: u64) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|s| s.id != subscriber_id);
    }
}

/// Per-session reader thread.  Reads PTY master, appends the chunk
/// to the disk bytelog (for reattach replay), fans each chunk out
/// as a DATA frame to all subscribers.  Exits on EOF; flips `alive`
/// to false so LIST reports it.
fn spawn_session_reader(session: Arc<ShellSession>) {
    let id = session.id;
    thread::Builder::new()
        .name(format!("shelld-pty-reader/{}", id))
        .spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                if SHUTDOWN.load(Ordering::Acquire) {
                    break;
                }
                match session.pty.read_shared(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Append to disk log FIRST so a crash between
                        // append + broadcast doesn't leave the GUI
                        // with state shelld can't replay.
                        if let Some(log) = session.bytelog.lock().unwrap().as_mut() {
                            let _ = log.append(&buf[..n]);
                        }
                        let frame = Frame::new(
                            MsgType::Data,
                            encode_data(session.id, &buf[..n]),
                        );
                        session.broadcast(frame);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            session.alive.store(false, Ordering::Release);
            // Distinguish "child genuinely exited (normal close)" from
            // "shelld is going down and dragged the child with it"
            // (Pty::Drop SIGHUP).  Same log tag both ways so existing
            // greps still match; the new field disambiguates root cause.
            let in_shutdown = SHUTDOWN.load(Ordering::Acquire);
            lx_info!(
                "session.reader.exit",
                "EOF on PTY master",
                id = id,
                in_shutdown = in_shutdown
            );
        })
        .expect("spawn session reader");
}

/// Global session table.  Sessions are looked up by id.  Drop of the
/// last `Arc<ShellSession>` triggers `Pty::Drop` (SIGHUP + waitpid +
/// close), so explicit Kill is just a `remove`.
type Sessions = Arc<Mutex<HashMap<u64, Arc<ShellSession>>>>;

/// On cold start, slide any staged binary in
/// `binaries/pending/marspot-shelld` into `current/`. No-op when nothing
/// is pending. Best-effort: never aborts the daemon — if anything goes
/// wrong (permission denied, rename failure, …) we log to stderr and
/// keep running the binary we were exec'd as, exactly as before. The
/// plist still points at the *path*, so the running image stays whatever
/// launchd loaded; the promote affects the *next* launch.
fn boot_promote_pending() {
    let tree = match marspot::binary_tree::BinaryTree::for_shelld() {
        Ok(t) => t,
        Err(e) => {
            lx_warn!("boot.promote.tree_init_failed", &format!("{e}"));
            return;
        }
    };
    if !tree.has_pending() {
        return;
    }
    lx_event!(
        "BOOT_PROMOTE_BEGIN",
        "pending found, promoting on cold start",
        pending = tree.pending().display()
    );
    match tree.promote_pending() {
        Ok(()) => lx_event!(
            "BOOT_PROMOTE_OK",
            "pending → current; next launchd start loads it",
            current = tree.current().display()
        ),
        Err(e) => lx_error!(
            "boot.promote.failed",
            &format!("{e}"),
            current = tree.current().display()
        ),
    }
}

/// Set / clear `FD_CLOEXEC` on a fd. Used pre-execv to mark the listen
/// fd + every PTY master fd as "survive the exec image swap"; used post-
/// fail to put them back the way they were so the running image can
/// keep operating.
fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Snapshot the post-execv image needs to rebuild Sessions + listener
/// without re-binding or losing the running shells.
struct Handoff {
    listen_fd: RawFd,
    next_session_id: u64,
    /// Per-session (id, master_fd, child_pid, title).  Bytelog path is
    /// derived from `session_id` (same `marspot::paths::sessions_dir()`
    /// layout either side of the swap).  Title persists user-set
    /// labels across the execv (v=2 SET_TITLE).
    sessions: Vec<(u64, RawFd, i32, String)>,
    /// Where the manifest file lived. New image deletes it after
    /// consuming so a future re-exec doesn't read stale state.
    manifest_path: PathBuf,
}

/// Detect "I was just exec'd by the previous shelld image as part of a
/// silent self-update" by looking for the three env vars the outgoing
/// image set before `execv`. None of them set → cold start (the
/// boot_promote_pending / bind path runs). All three set → resume.
///
/// On any parse failure we treat the handoff as missing and fall back
/// to cold start; the LaunchAgent's KeepAlive will keep us upright,
/// at the cost of dropped sessions.
fn try_resume_handoff() -> Option<Handoff> {
    let path = std::env::var_os("MARSPOT_SHELLD_HANDOFF")?;
    let manifest_path = PathBuf::from(path);
    let listen_fd: RawFd = std::env::var("MARSPOT_SHELLD_LISTEN_FD")
        .ok()?
        .parse()
        .ok()?;
    let next_session_id: u64 = std::env::var("MARSPOT_SHELLD_NEXT_SESSION_ID")
        .ok()?
        .parse()
        .ok()?;
    // Clear so a future cold-start (e.g. launchd KeepAlive after an
    // unrelated crash that happens to inherit our env) doesn't pick up
    // a stale handoff with already-closed fds.
    unsafe {
        std::env::remove_var("MARSPOT_SHELLD_HANDOFF");
        std::env::remove_var("MARSPOT_SHELLD_LISTEN_FD");
        std::env::remove_var("MARSPOT_SHELLD_NEXT_SESSION_ID");
    }

    let contents = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            lx_error!(
                "execv.handoff.manifest_read_failed",
                &format!("falling back to cold start: {e}"),
                manifest = manifest_path.display()
            );
            return None;
        }
    };
    let mut sessions: Vec<(u64, RawFd, i32, String)> = Vec::new();
    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let id: u64 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => {
                lx_error!("execv.handoff.bad_id", line);
                return None;
            }
        };
        let fd: RawFd = match parts.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => {
                lx_error!("execv.handoff.bad_fd", line, id = id);
                return None;
            }
        };
        let pid: i32 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => {
                lx_error!("execv.handoff.bad_pid", line, id = id, fd = fd);
                return None;
            }
        };
        // Title column added in v=2.  Older manifests (pre-2026-06-15)
        // had no 4th column — treat missing as "no custom title".
        let title = parts.next().unwrap_or("").to_string();
        sessions.push((id, fd, pid, title));
    }
    Some(Handoff {
        listen_fd,
        next_session_id,
        sessions,
        manifest_path,
    })
}

/// Per-pid handoff manifest path. PID is preserved across execv, so the
/// new image can derive the same path without it being passed in env.
/// (It IS also passed in env — that's the source of truth — but using
/// pid keeps the temp file unique across parallel shelld instances in
/// a dev sandbox.)
fn handoff_manifest_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "marspot-shelld-handoff.{}.tsv",
        std::process::id()
    ))
}

/// Promote pending → current, write the handoff manifest, clear CLOEXEC
/// on listen + master fds, and `execv` over our own image. On success
/// this function does not return; on any failure it rolls back the
/// binary tree (prev → current), restores CLOEXEC, and returns the
/// error so the caller can keep running on the current image.
fn do_execv_swap(sessions: &Sessions, listen_fd: RawFd) -> io::Result<()> {
    let started = std::time::Instant::now();
    let tree = marspot::binary_tree::BinaryTree::for_shelld()?;
    if !tree.has_pending() {
        lx_info!(
            "execv.skip",
            "SIGUSR1 received but no pending binary; ignoring"
        );
        return Ok(());
    }

    // Snapshot sessions BEFORE we touch the tree, so a failure
    // promoting doesn't leave us with a half-written manifest.
    // Includes the user-set title so it survives the execv handoff
    // — without this the v=2 SetTitle persistence is silently
    // undone any time shelld self-updates.
    let snapshot: Vec<(u64, RawFd, i32, String)> = {
        let g = sessions.lock().unwrap();
        let mut v: Vec<(u64, RawFd, i32, String)> = g
            .values()
            .map(|s| {
                let title = s.title.lock().unwrap().clone();
                (s.id, s.pty.raw_master(), s.pty.child_pid(), title)
            })
            .collect();
        v.sort_by_key(|t| t.0);
        v
    };
    let next_session_id = NEXT_SESSION_ID.load(Ordering::Acquire);
    lx_event!(
        "EXECV_HANDOFF_BEGIN",
        "snapshotted sessions for handoff manifest",
        pid = std::process::id(),
        listen_fd = listen_fd,
        n_sessions = snapshot.len(),
        next_id = next_session_id
    );
    for (id, fd, child_pid, title) in &snapshot {
        lx_debug!(
            "execv.handoff.session",
            "session in manifest",
            id = id,
            master_fd = fd,
            child_pid = child_pid,
            title_len = title.len()
        );
    }

    // Write manifest. 0o600 so other users can't peek at fd numbers.
    // Tabs and newlines in title would collide with the line/column
    // delimiters; commit_title_edit already trims whitespace but be
    // defensive — replace any sneak-in with a space.
    let manifest_path = handoff_manifest_path();
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&manifest_path)?;
        for (id, fd, pid, title) in &snapshot {
            let safe_title: String = title
                .chars()
                .map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            writeln!(f, "{}\t{}\t{}\t{}", id, fd, pid, safe_title)?;
        }
        f.sync_all().ok();
    }

    // Promote: pending/<bin> → current/<bin> (and current → prev if it
    // existed). This is the only filesystem-changing step that runs
    // before execv; everything else above is in /tmp.
    tree.promote_pending()?;
    let target = tree.current();
    lx_event!(
        "EXECV_PROMOTE_OK",
        "promoted pending → current",
        target = target.display()
    );

    // Mark every fd the new image needs to inherit as "survive exec".
    // Listen fd: kernel-side socket stays bound to the same socket
    // path, so clients reconnecting hit the new image transparently.
    // Master fds: the running zsh children at the other end keep their
    // PTY pair alive — they have no idea anything happened.
    clear_cloexec(listen_fd)?;
    let mut cloexec_master_fail = 0usize;
    for (_, fd, _, _) in &snapshot {
        if let Err(e) = clear_cloexec(*fd) {
            // Best-effort: if we can't clear CLOEXEC on a master fd,
            // the session won't survive the swap. Log and continue —
            // an offline session is better than a panicked daemon.
            cloexec_master_fail += 1;
            lx_warn!(
                "execv.clear_cloexec_failed",
                &format!("{e}"),
                master_fd = fd
            );
        }
    }

    // Hand env to the new image. `MARSPOT_SHELLD_HANDOFF` is the
    // detection sentinel; the other two are payload.
    unsafe {
        std::env::set_var("MARSPOT_SHELLD_HANDOFF", &manifest_path);
        std::env::set_var("MARSPOT_SHELLD_LISTEN_FD", listen_fd.to_string());
        std::env::set_var(
            "MARSPOT_SHELLD_NEXT_SESSION_ID",
            next_session_id.to_string(),
        );
    }

    let target_c =
        CString::new(target.to_string_lossy().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL")
        })?;
    let argv: [*const libc::c_char; 2] = [target_c.as_ptr(), std::ptr::null()];

    lx_event!(
        "EXECV_INVOKE",
        "calling libc::execv — outgoing image yields here",
        target = target.display(),
        listen_fd = listen_fd,
        n_sessions = snapshot.len(),
        cloexec_master_fail = cloexec_master_fail,
        elapsed_us = started.elapsed().as_micros()
    );
    // SAFETY: target_c outlives the call; on success execv does not
    // return so the borrow is irrelevant. On failure we surface the
    // errno and let the caller restore state.
    unsafe {
        libc::execv(target_c.as_ptr(), argv.as_ptr());
    }
    let err = io::Error::last_os_error();
    let errno = err.raw_os_error().unwrap_or(0);
    lx_error!(
        "execv.failed",
        &format!("{err}"),
        errno = errno,
        target = target.display(),
        elapsed_us = started.elapsed().as_micros()
    );

    // Rollback EVERYTHING so the running image stays usable. Reverse
    // order matches the forward path:
    //   1. clear env vars (so a manual restart doesn't see them stale)
    //   2. restore CLOEXEC on listen + master fds
    //   3. delete manifest
    //   4. roll the binary tree back: prev → current, current → quarantine
    unsafe {
        std::env::remove_var("MARSPOT_SHELLD_HANDOFF");
        std::env::remove_var("MARSPOT_SHELLD_LISTEN_FD");
        std::env::remove_var("MARSPOT_SHELLD_NEXT_SESSION_ID");
    }
    let _ = set_cloexec(listen_fd);
    for (_, fd, _, _) in &snapshot {
        let _ = set_cloexec(*fd);
    }
    let _ = std::fs::remove_file(&manifest_path);
    let rollback_result = tree.rollback_to_prev();
    if let Err(rollback_err) = &rollback_result {
        lx_error!(
            "execv.rollback_failed",
            &format!("{rollback_err}"),
            errno = errno
        );
    }
    lx_event!(
        "EXECV_ROLLBACK_DONE",
        "stayed on outgoing image",
        errno = errno,
        rolled_back = rollback_result.is_ok()
    );
    Err(err)
}

/// Construct the read+write fds of a self-pipe, mark both CLOEXEC.
/// The write end goes into `EXEC_WAKE_FD` so the signal handler can
/// wake the accept loop; the read end is part of the loop's poll set.
fn make_self_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let (r_fd, w_fd) = (fds[0], fds[1]);
    set_cloexec(r_fd).ok();
    set_cloexec(w_fd).ok();
    Ok((r_fd, w_fd))
}

/// Rebuild every session from a handoff manifest: wrap each inherited
/// master fd into a `Pty`, open the on-disk bytelog (append mode), and
/// spawn the reader thread. Subscribers start empty; GUI / L2 clients
/// reattach via the still-bound socket and `replay()` rebuilds their
/// terminal state from the bytelog. Best-effort per session — a session
/// that fails to rehydrate (bytelog open error, etc.) is logged and
/// skipped rather than aborting the whole daemon.
fn rehydrate_sessions(
    sessions: &Sessions,
    snapshot: &[(u64, RawFd, i32, String)],
) -> usize {
    let mut count = 0usize;
    for (id, fd, pid, title) in snapshot {
        let pty = Arc::new(Pty::from_raw_master(*fd, *pid));
        let bytelog = match ByteLog::open(*id) {
            Ok(b) => Some(b),
            Err(e) => {
                lx_warn!(
                    "execv.rehydrate.bytelog_open_failed",
                    &format!("{e}"),
                    id = id,
                    master_fd = fd
                );
                None
            }
        };
        let session = Arc::new(ShellSession {
            id: *id,
            pty,
            subscribers: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            bytelog: Mutex::new(bytelog),
            title: Mutex::new(title.clone()),
        });
        sessions.lock().unwrap().insert(*id, session.clone());
        spawn_session_reader(session);
        count += 1;
    }
    count
}

fn main() {
    // --log-event TAG DETAIL: tiny CLI shim used by bash callers
    // (install-shelld.sh's sup_log) so structured supervisor events
    // emitted from shell scripts land in the same TSV stream as the
    // daemon's own events. Initialise logx then emit one Info event
    // and exit.
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() >= 4 && argv[1] == "--log-event" {
        marspot::logx::init("shelld");
        marspot::logx::event(
            marspot::logx::Level::Info,
            &argv[2],
            &argv[3],
            &[],
        );
        return;
    }
    // Test hook: `marspot-shelld --log-soak <count>` emits `<count>`
    // structured events from a single long-lived process so the
    // rotation code path (which triggers every 256 writes or on
    // size-cap hit) is actually exercised. Used by bin/soak-log-rotate.sh
    // to stress the flock-coordinated cross-process rotate + compress.
    if argv.len() >= 3 && argv[1] == "--log-soak" {
        let count: u64 = argv[2].parse().unwrap_or(0);
        marspot::logx::init("shelld");
        for i in 0..count {
            lx_info!(
                "log_soak.tick",
                "soak event filler",
                seq = i,
                payload = "----------xxxxxxxxxx----------xxxxxxxxxx----------"
            );
        }
        return;
    }

    marspot::logx::init("shelld");
    lx_event!(
        "SHELLD_START",
        "daemon main started",
        version_shelld = env!("MARSPOT_VERSION_SHELLD"),
        version_core = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        pid = std::process::id()
    );

    // Detect "we were just exec'd by the previous shelld image as part
    // of an in-place self-update". If yes, we skip bind + boot-promote
    // and rehydrate every session from the inherited fds. If no, this
    // is a normal cold start; we promote any pending binary then bind.
    let handoff = try_resume_handoff();

    if handoff.is_none() {
        // Cold-start boot-promote. If the updater (or install-shelld.sh)
        // staged a new binary in binaries/pending/marspot-shelld, slide it
        // into current/ before we do anything else. The LaunchAgent plist
        // is expected to point at current/marspot-shelld (install-shelld.sh
        // sets this up); this means a manually-dropped pending followed by
        // `launchctl kickstart -k com.marspot.shelld` lands on the new
        // image automatically, without anyone running --apply-pending.
        //
        // Hot path (SIGUSR1 execv self-update) does its own in-process
        // promote_pending(); we skip this branch in the handoff resume
        // case because that promote already ran on the outgoing image.
        boot_promote_pending();
    }

    // launchd-launched daemons inherit a minimal env — TERM is
    // commonly `network` (macOS launchd default) or unset, which
    // makes zsh + terminfo derive bogus terminal capabilities.
    // Concrete symptom: `backward-delete-char` widget emits only a
    // space instead of `\b \b`, so backspace looks like it "writes
    // spaces" in marspot.  Override before any child spawn so every
    // forked zsh sees a sane TERM.  Same heuristic as the marspot-
    // side shell shim: leave alone if already a usable value.
    let term_ok = std::env::var("TERM")
        .map(|t| !t.is_empty() && t != "network" && t != "dumb" && t != "unknown")
        .unwrap_or(false);
    if !term_ok {
        unsafe { std::env::set_var("TERM", "xterm-256color") };
    }
    // Advertise 24-bit colour.  Without COLORTERM, apps (Claude Code, vim,
    // bat, …) downgrade truecolor to the 256-colour cube, whose
    // approximations visibly shift hues — Claude's coral orange lands on
    // cube index 174 = (215,135,135), a rose/pink ("水红").  marspot's
    // parser renders 38;2;r;g;b truecolor, so claim it.
    if std::env::var_os("COLORTERM").is_none() {
        unsafe { std::env::set_var("COLORTERM", "truecolor") };
    }
    // Same for ZDOTDIR: marspot's lib installs a shim under
    // `~/.cache/marspot/zdot` (PROMPT_SP, EOL_MARK).  shelld is a
    // separate process so we re-run the install once on startup.
    // `install_shell_zdot_shim` is idempotent and cheap.
    marspot::session::ensure_zdot_shim_for_external_shells();

    let sock = socket_path();
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    // Cold start → bind a fresh listening socket. Handoff resume →
    // inherit the listen fd from the outgoing image (kernel-side socket
    // is unchanged; clients reconnecting hit us transparently).
    let raw: RawFd = match handoff.as_ref() {
        Some(h) => {
            lx_event!(
                "EXECV_RESUME_BEGIN",
                "handoff env detected; inheriting listen fd",
                pid = std::process::id(),
                listen_fd = h.listen_fd,
                n_sessions = h.sessions.len(),
                next_id = h.next_session_id
            );
            h.listen_fd
        }
        None => {
            if let Some(parent) = sock.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    lx_error!(
                        "bind.mkdir_failed",
                        &format!("{e}"),
                        dir = parent.display()
                    );
                    std::process::exit(1);
                }
            }
            let _ = std::fs::remove_file(&sock);
            let listener = match UnixListener::bind(&sock) {
                Ok(l) => l,
                Err(e) => {
                    lx_error!("bind.failed", &format!("{e}"), sock = sock.display());
                    std::process::exit(1);
                }
            };
            if let Err(e) =
                std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
            {
                lx_warn!("bind.chmod_failed", &format!("{e}"), sock = sock.display());
            }
            lx_info!("bind.ok", "listening on socket", sock = sock.display());
            listener.into_raw_fd()
        }
    };
    LISTENER_FD.store(raw, Ordering::Release);

    // Rehydrate sessions from the inherited fds BEFORE we install signal
    // handlers — a stray SIGUSR1 during rehydrate would otherwise see an
    // empty sessions table and re-execv with no manifest.
    if let Some(h) = handoff {
        NEXT_SESSION_ID.store(h.next_session_id, Ordering::Release);
        let count = rehydrate_sessions(&sessions, &h.sessions);
        // Manifest no longer needed; delete so a future spurious env
        // re-trigger doesn't try to read the same fd numbers (which by
        // then point at different files in the kernel fd table).
        let _ = std::fs::remove_file(&h.manifest_path);
        lx_event!(
            "EXECV_RESUME_DONE",
            "session rehydrate complete",
            rehydrated = count,
            total = h.sessions.len()
        );
    }

    // Self-pipe to wake the accept loop on SIGUSR1. CLOEXEC on both
    // ends so the kernel drops them on the next execv (new image
    // creates its own pair).
    let (wake_r, wake_w) = match make_self_pipe() {
        Ok(pair) => pair,
        Err(e) => {
            lx_error!("self_pipe.create_failed", &format!("{e}"));
            std::process::exit(1);
        }
    };
    EXEC_WAKE_FD.store(wake_w, Ordering::Release);

    install_signal_handlers();

    // Schedule periodic GC of cold data (rotated logs > 7d, orphan
    // bytelogs > 1h grace, binaries/{prev>7d, quarantine>30d}, launchd
    // shelld.{log,err} tail-trim > 16 MiB). Run one sweep right at boot
    // so freshly-rebooted daemons don't accumulate, then every 6 h from
    // the event loop.
    let mut last_gc = std::time::Instant::now();
    {
        let live = marspot::logx::gc::LiveSet {
            session_ids: sessions.lock().unwrap().keys().copied().collect(),
        };
        std::thread::spawn(move || marspot::logx::gc::sweep_full(&live));
    }
    let gc_interval = std::time::Duration::from_secs(6 * 3600);

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            // Attribute the shutdown to its trigger.  signal_handler
            // wrote LAST_SIGNAL before flipping SHUTDOWN; reading it
            // here lets the operator distinguish "SIGTERM from
            // launchctl bootout" (the cascade we saw on 2026-06-15)
            // from "SIGINT from terminal" from "shutdown raised
            // internally" (LAST_SIGNAL == 0).
            let sig = LAST_SIGNAL.load(Ordering::Acquire);
            let alive_ids: Vec<u64> = {
                let g = sessions.lock().unwrap();
                g.iter()
                    .filter(|(_, s)| s.alive.load(Ordering::Acquire))
                    .map(|(id, _)| *id)
                    .collect()
            };
            let alive_count = alive_ids.len();
            let total_count = sessions.lock().unwrap().len();
            lx_event!(
                "SHELLD_SHUTDOWN_OBSERVED",
                "main loop observed SHUTDOWN flag — preparing to teardown",
                signal = sig,
                signal_name = match sig {
                    libc::SIGTERM => "SIGTERM",
                    libc::SIGINT => "SIGINT",
                    0 => "<internal>",
                    _ => "<other>",
                },
                alive_sessions = alive_count,
                total_sessions = total_count,
                alive_ids = format!("{:?}", alive_ids)
            );
            break;
        }
        if EXEC_TRIGGER.swap(false, Ordering::AcqRel) {
            // do_execv_swap on success replaces this image; on failure
            // returns Err and rolls back so we keep serving.
            if let Err(e) = do_execv_swap(&sessions, raw) {
                lx_warn!("execv.aborted", &format!("{e}"));
            }
            continue;
        }
        // 6-hour cold-data sweep. Detached thread so the accept loop
        // doesn't block on readdir/unlink syscalls.
        if last_gc.elapsed() > gc_interval {
            let live = marspot::logx::gc::LiveSet {
                session_ids: sessions.lock().unwrap().keys().copied().collect(),
            };
            std::thread::spawn(move || marspot::logx::gc::sweep_full(&live));
            last_gc = std::time::Instant::now();
        }

        // poll on (listen, wake) so a signal-driven wake doesn't sit
        // behind a blocking accept. std's accept silently retries
        // EINTR, so a signal alone wouldn't unstick it — the wake
        // byte is what we rely on.
        let mut pfds = [
            libc::pollfd {
                fd: raw,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_r,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            lx_error!("poll.failed", &format!("{err}"));
            break;
        }
        if pfds[1].revents != 0 {
            let mut drain = [0u8; 16];
            unsafe {
                libc::read(wake_r, drain.as_mut_ptr() as _, drain.len());
            }
            continue;
        }
        if pfds[0].revents & (libc::POLLIN as i16) != 0 {
            // SAFETY: raw is our owned fd; we don't drop the wrapper.
            let wrapper = unsafe { UnixListener::from_raw_fd(raw) };
            let accept_result = wrapper.accept();
            std::mem::forget(wrapper);
            match accept_result {
                Ok((s, _)) => {
                    let sess = sessions.clone();
                    thread::spawn(move || handle_client(s, sess));
                }
                Err(_) => {
                    if SHUTDOWN.load(Ordering::Acquire) {
                        break;
                    }
                    lx_warn!("accept.unexpected_error", "ending accept loop");
                    break;
                }
            }
        }
    }
    let fd = LISTENER_FD.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    let wake_fd = EXEC_WAKE_FD.swap(-1, Ordering::AcqRel);
    if wake_fd >= 0 {
        unsafe { libc::close(wake_fd) };
    }
    unsafe { libc::close(wake_r) };
    // Snapshot what we're about to take down with us.  Every alive
    // session entry below corresponds to a claudecode-or-equivalent
    // child that is about to receive SIGHUP via `Pty::Drop` when
    // the HashMap drops.  Capturing the list here makes a
    // shelld-killed-N-active-sessions event reconstructable from
    // marspot.log alone (no need to grep stderr or guess from
    // child death signals upstream).
    let about_to_die: Vec<u64> = {
        let g = sessions.lock().unwrap();
        g.iter()
            .filter(|(_, s)| s.alive.load(Ordering::Acquire))
            .map(|(id, _)| *id)
            .collect()
    };
    lx_event!(
        "SHELLD_STOP",
        "daemon shutting down; alive sessions will be SIGHUP'd via Pty::Drop",
        about_to_sighup = about_to_die.len(),
        ids = format!("{:?}", about_to_die)
    );
    let _ = std::fs::remove_file(&sock);
    // Sessions table drops here, which drops each Arc<ShellSession>,
    // which drops Pty, which SIGHUPs + waits the children.
}

/// Per-client handler.  Splits the socket into reader + writer halves
/// (clone gives us a second fd via `dup`) so the reader can process
/// commands and the writer can stream DATA frames concurrently.
fn handle_client(stream: UnixStream, sessions: Sessions) {
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            lx_error!("client.try_clone_failed", &format!("{e}"));
            return;
        }
    };
    let (out_tx, out_rx) = mpsc::sync_channel::<Frame>(SUBSCRIBER_QUEUE_DEPTH);
    let writer_handle = thread::Builder::new()
        .name("shelld-client-writer".into())
        .spawn(move || writer_loop(writer_stream, out_rx))
        .expect("spawn writer");

    // Track which sessions THIS client is attached to so we detach
    // them on disconnect (otherwise a dead sender accumulates in
    // session.subscribers until the next broadcast prunes it).
    // Pair is (session, this-client's subscriber id on that session).
    let mut attached: Vec<(Weak<ShellSession>, u64)> = Vec::new();
    let attached_tx = out_tx.clone();

    reader_loop(stream, &out_tx, &sessions, &mut attached, &attached_tx);

    // Detach from any sessions this client was on.
    for (w, sub_id) in attached {
        if let Some(s) = w.upgrade() {
            s.detach(sub_id);
        }
    }
    drop(out_tx);
    let _ = writer_handle.join();
}

fn writer_loop(mut stream: UnixStream, rx: Receiver<Frame>) {
    while let Ok(frame) = rx.recv() {
        if frame.write_to(&mut stream).is_err() {
            break;
        }
    }
}

fn reader_loop(
    mut stream: UnixStream,
    out_tx: &SyncSender<Frame>,
    sessions: &Sessions,
    attached: &mut Vec<(Weak<ShellSession>, u64)>,
    attached_tx: &SyncSender<Frame>,
) {
    let mut handshook = false;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
        let frame = match Frame::read_from(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                lx_warn!("client.read_error", &format!("{e}"));
                let _ = out_tx.send(err_frame(1, &format!("read: {}", e)));
                break;
            }
        };
        if !handshook && frame.msg_type != MsgType::Hello {
            let _ = out_tx.send(err_frame(2, "first frame must be HELLO"));
            break;
        }
        match frame.msg_type {
            MsgType::Hello => {
                let v = match decode_hello(&frame.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(3, &format!("bad HELLO: {}", e)));
                        break;
                    }
                };
                if v != PROTO_VERSION {
                    let _ = out_tx.send(err_frame(
                        4,
                        &format!(
                            "proto version mismatch: client={} server={}",
                            v, PROTO_VERSION
                        ),
                    ));
                    break;
                }
                let _ = out_tx.send(Frame::new(
                    MsgType::HelloAck,
                    encode_hello_ack(PROTO_VERSION, [0u8; 8]),
                ));
                handshook = true;
            }
            MsgType::ListSessions => {
                let snapshot: Vec<SessionInfo> = {
                    let g = sessions.lock().unwrap();
                    g.values()
                        .map(|s| SessionInfo {
                            session_id: s.id,
                            child_pid: s.pty.child_pid(),
                            alive: s.alive.load(Ordering::Acquire),
                            title: s.title.lock().unwrap().clone(),
                        })
                        .collect()
                };
                let _ = out_tx.send(Frame::new(
                    MsgType::ListSessionsReply,
                    encode_list_sessions_reply(&snapshot),
                ));
            }
            MsgType::SetTitle => {
                match marspot::shelld_proto::decode_set_title(&frame.payload) {
                    Ok((id, title)) => {
                        let g = sessions.lock().unwrap();
                        if let Some(s) = g.get(&id) {
                            *s.title.lock().unwrap() = title.to_string();
                        } else {
                            // Unknown session — drop silently; the client
                            // will retry after attach completes if it
                            // raced session creation.
                            lx_warn!(
                                "shelld.set_title.unknown_session",
                                "SET_TITLE for unknown session_id",
                                id = id
                            );
                        }
                    }
                    Err(e) => {
                        let _ = out_tx.send(err_frame(7, &format!("bad SET_TITLE: {e}")));
                    }
                }
            }
            MsgType::NewSession => {
                let (cols, rows, cwd) = match decode_new_session(&frame.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(6, &format!("bad NEW_SESSION: {}", e)));
                        continue;
                    }
                };
                match spawn_shell(cols, rows, &cwd) {
                    Ok(pty) => {
                        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::AcqRel);
                        let child_pid = pty.child_pid();
                        let bytelog = match ByteLog::open(id) {
                            Ok(b) => Some(b),
                            Err(e) => {
                                lx_warn!(
                                    "session.bytelog_open_failed",
                                    &format!("running without replay: {e}"),
                                    id = id
                                );
                                None
                            }
                        };
                        let session = Arc::new(ShellSession {
                            id,
                            pty: Arc::new(pty),
                            subscribers: Mutex::new(Vec::new()),
                            alive: AtomicBool::new(true),
                            bytelog: Mutex::new(bytelog),
                            title: Mutex::new(String::new()),
                        });
                        // Auto-attach the creator before publishing —
                        // any DATA the reader thread emits before
                        // NEW_SESSION_REPLY lands at the client must
                        // not be dropped.
                        let sub_id = session.attach(attached_tx.clone());
                        attached.push((Arc::downgrade(&session), sub_id));
                        sessions.lock().unwrap().insert(id, session.clone());
                        spawn_session_reader(session);
                        let _ = out_tx.send(Frame::new(
                            MsgType::NewSessionReply,
                            encode_new_session_reply(id, child_pid),
                        ));
                    }
                    Err(e) => {
                        let _ = out_tx.send(err_frame(7, &format!("spawn failed: {}", e)));
                    }
                }
            }
            MsgType::Attach => {
                let id = match decode_session_id(&frame.payload) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(8, &format!("bad ATTACH: {}", e)));
                        continue;
                    }
                };
                let session = sessions.lock().unwrap().get(&id).cloned();
                match session {
                    Some(s) => {
                        // Critical ordering: hold the bytelog mutex
                        // across attach + replay so the per-session
                        // reader thread (which takes the same lock
                        // before each append+broadcast) can't slip
                        // a live broadcast in between us subscribing
                        // and finishing the historical replay.
                        // Subscriber gets historical bytes first,
                        // then live; no overlap, no gap.
                        let log_guard = s.bytelog.lock().unwrap();
                        let sub_id = s.attach(attached_tx.clone());
                        attached.push((Arc::downgrade(&s), sub_id));
                        if let Some(log) = log_guard.as_ref() {
                            if let Err(e) = log.replay(id, attached_tx) {
                                lx_warn!(
                                    "session.replay_failed",
                                    &format!("{e}"),
                                    id = id
                                );
                            }
                        }
                        drop(log_guard);
                    }
                    None => {
                        let _ =
                            out_tx.send(err_frame(9, &format!("session {} not found", id)));
                    }
                }
            }
            MsgType::Detach => {
                let id = match decode_session_id(&frame.payload) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(10, &format!("bad DETACH: {}", e)));
                        continue;
                    }
                };
                if let Some(s) = sessions.lock().unwrap().get(&id).cloned() {
                    // Find this client's subscriber id on that session,
                    // detach it, and forget the entry.
                    if let Some(pos) = attached.iter().position(|(w, _)| {
                        w.upgrade().map(|a| a.id == id).unwrap_or(false)
                    }) {
                        let (_, sub_id) = attached.remove(pos);
                        s.detach(sub_id);
                    }
                }
            }
            MsgType::Kill => {
                let id = match decode_session_id(&frame.payload) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(11, &format!("bad KILL: {}", e)));
                        continue;
                    }
                };
                sessions.lock().unwrap().remove(&id);
                // Drop on the removed Arc cascades into Pty::Drop —
                // SIGHUP + waitpid + close.  Subscribers' next
                // broadcast attempt finds nothing (we just removed),
                // and their reader thread observed EOF on the closed
                // PTY master.
                attached.retain(|(w, _)| {
                    w.upgrade().map(|a| a.id != id).unwrap_or(false)
                });
                // The bytelog file lives on disk; KILL means the
                // GUI is done with this session, so drop the
                // history with it.  Detach-without-kill keeps the
                // log alive for future reattach.
                delete_bytelog(id);
            }
            MsgType::Resize => {
                let (id, cols, rows) = match decode_resize(&frame.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(12, &format!("bad RESIZE: {}", e)));
                        continue;
                    }
                };
                if let Some(s) = sessions.lock().unwrap().get(&id).cloned() {
                    // Pty::resize takes &mut self; we have Arc<Pty>.
                    // TIOCSWINSZ is a single ioctl and the kernel
                    // serialises ioctl on a single fd, so doing it
                    // directly with libc is fine.  Avoids needing to
                    // duplicate the Pty::resize body just for &self.
                    let ws = libc::winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe { libc::ioctl(s.pty.raw_master(), libc::TIOCSWINSZ, &ws) };
                }
            }
            MsgType::Input => {
                let (id, bytes) = match decode_data(&frame.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = out_tx.send(err_frame(13, &format!("bad INPUT: {}", e)));
                        continue;
                    }
                };
                if let Some(s) = sessions.lock().unwrap().get(&id).cloned() {
                    let n = s.pty.write_shared(bytes).unwrap_or(0);
                    if let Ok(home) = std::env::var("HOME") {
                        if std::path::Path::new(&format!("{}/.marspot-trace", home)).exists() {
                            use std::io::Write as _;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true).append(true)
                                .open("/tmp/marspot-shelld-input-trace.log")
                            {
                                let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                                let _ = writeln!(
                                    f,
                                    "session={} requested={} wrote={} [{}]",
                                    id, bytes.len(), n, hex
                                );
                            }
                        }
                    }
                }
            }
            other => {
                let _ = out_tx.send(err_frame(
                    5,
                    &format!("unimplemented msg_type {:?}", other),
                ));
                break;
            }
        }
    }
}

fn err_frame(code: u32, msg: &str) -> Frame {
    Frame::new(MsgType::Error, encode_error(code, msg))
}

/// Shell spawn from inside shelld.  Unlike `Session::spawn` we
/// deliberately skip the `/usr/bin/login -fpl` wrapper: under
/// launchd's per-user session, `login(1)` doesn't fully reset the
/// controlling terminal in the way an interactive child shell
/// expects (manifests as backspace / arrow keys not reaching
/// readline).  Direct exec of `$SHELL` with a leading `-` in argv[0]
/// tells the shell to behave as a login shell, which is all the
/// behaviour we actually wanted login(1) for.
fn spawn_shell(cols: u16, rows: u16, cwd_override: &str) -> io::Result<Pty> {
    let cwd = if cwd_override.is_empty() {
        std::env::var("HOME").ok()
    } else {
        Some(cwd_override.to_string())
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let argv0 = format!(
        "-{}",
        std::path::Path::new(&shell)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sh")
    );
    Pty::spawn(PtyConfig {
        program: shell,
        args: Vec::new(),
        size: TerminalSize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        },
        argv0: Some(argv0),
        cwd,
    })
}

extern "C" fn signal_handler(sig: libc::c_int) {
    // Record signal BEFORE flipping SHUTDOWN — the main loop checks
    // SHUTDOWN first then reads LAST_SIGNAL; this ordering guarantees
    // the read sees the signal that caused the flip.
    LAST_SIGNAL.store(sig as i32, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
    let msg: &[u8] = match sig {
        libc::SIGTERM => b"[shelld] SIGTERM\n",
        libc::SIGINT => b"[shelld] SIGINT\n",
        _ => b"[shelld] signal\n",
    };
    unsafe {
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as _, msg.len());
    }
    let fd = LISTENER_FD.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// SIGUSR1 = "apply pending shelld via execv self-update". Set the
/// trigger flag and wake the accept loop via the self-pipe. All work
/// (promote, manifest, execv) happens in main-thread context — handler
/// only touches async-signal-safe state.
extern "C" fn sigusr1_handler(_sig: libc::c_int) {
    EXEC_TRIGGER.store(true, Ordering::Release);
    let fd = EXEC_WAKE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let b: [u8; 1] = [b'!'];
        unsafe {
            libc::write(fd, b.as_ptr() as _, 1);
        }
    }
}

fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        let r1 = libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        let r2 = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        if r1 != 0 || r2 != 0 {
            lx_error!(
                "sigaction.failed",
                "could not install TERM/INT handlers",
                term = r1,
                int = r2,
                errno = io::Error::last_os_error().raw_os_error().unwrap_or(0)
            );
        }
        let mut sa_usr1: libc::sigaction = std::mem::zeroed();
        sa_usr1.sa_sigaction = sigusr1_handler as *const () as usize;
        libc::sigemptyset(&mut sa_usr1.sa_mask);
        // SA_RESTART so the in-flight write/read in reader threads
        // doesn't bubble EINTR up the bytelog path — only the
        // explicit poll in main reacts to the wake byte.
        sa_usr1.sa_flags = libc::SA_RESTART;
        let r3 = libc::sigaction(libc::SIGUSR1, &sa_usr1, std::ptr::null_mut());
        if r3 != 0 {
            lx_error!(
                "sigaction.usr1_failed",
                "could not install SIGUSR1 handler",
                errno = io::Error::last_os_error().raw_os_error().unwrap_or(0)
            );
        }
    }
}
