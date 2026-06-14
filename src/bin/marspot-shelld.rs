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
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write as IoWrite};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use marspot::pty::{Pty, PtyConfig, TerminalSize};
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
/// Raw fd of the listening socket, published after `bind` so the
/// signal handler can `close(2)` it (AS-safe per POSIX) and force
/// the in-flight `accept` to return EBADF.  Negative sentinel before
/// init or after close.
static LISTENER_FD: AtomicI32 = AtomicI32::new(-1);
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
            eprintln!("[shelld] session {} reader exiting", id);
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
            eprintln!("[shelld] boot-promote: tree init failed: {e}");
            return;
        }
    };
    if !tree.has_pending() {
        return;
    }
    eprintln!(
        "[shelld] boot-promote: pending found at {}, promoting",
        tree.pending().display()
    );
    match tree.promote_pending() {
        Ok(()) => eprintln!(
            "[shelld] boot-promote: pending → current ({}). next launchd start will load it.",
            tree.current().display()
        ),
        Err(e) => eprintln!("[shelld] boot-promote: promote_pending failed: {e}"),
    }
}

fn main() {
    eprintln!("[shelld] starting (pid={})", std::process::id());

    // Cold-start boot-promote. If the updater (or install-shelld.sh) staged
    // a new binary in binaries/pending/marspot-shelld, slide it into
    // current/ before we do anything else. The LaunchAgent plist is
    // expected to point at current/marspot-shelld (install-shelld.sh sets
    // this up); this means a manually-dropped pending followed by
    // `launchctl kickstart -k com.marspot.shelld` lands on the new image
    // automatically, without anyone running --apply-pending.
    //
    // Hot path (SIGUSR1 execv self-update) does its own in-process
    // promote_pending() so the running image swaps without restart; see
    // step 4. This boot path is the cold-start safety net for cases where
    // we DID restart (manual or crash recovery) and a pending was sitting
    // around.
    boot_promote_pending();

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
    if let Some(parent) = sock.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[shelld] failed to create cache dir {}: {}", parent.display(), e);
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[shelld] bind {} failed: {}", sock.display(), e);
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("[shelld] chmod {} failed: {}", sock.display(), e);
    }
    eprintln!("[shelld] listening on {}", sock.display());

    let raw = listener.into_raw_fd();
    LISTENER_FD.store(raw, Ordering::Release);
    install_signal_handlers();

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
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
                eprintln!("[shelld] accept error (unexpected)");
                break;
            }
        }
    }
    let fd = LISTENER_FD.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    eprintln!("[shelld] shutting down");
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
            eprintln!("[shelld] try_clone failed: {}", e);
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
                eprintln!("[shelld] read error: {}", e);
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
                            title: String::new(),
                        })
                        .collect()
                };
                let _ = out_tx.send(Frame::new(
                    MsgType::ListSessionsReply,
                    encode_list_sessions_reply(&snapshot),
                ));
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
                                eprintln!(
                                    "[shelld] open bytelog for session {} failed: {} (running without replay)",
                                    id, e
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
                                eprintln!(
                                    "[shelld] replay session {} failed: {}",
                                    id, e
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

fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        let r1 = libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        let r2 = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        if r1 != 0 || r2 != 0 {
            eprintln!(
                "[shelld] sigaction failed: TERM={} INT={} errno={}",
                r1,
                r2,
                io::Error::last_os_error()
            );
        }
    }
}
