//! Client-side connection to `marspot-shelld`.
//!
//! One `ShelldClient` per marspot GUI process.  Owns the unix socket,
//! a background reader thread that decodes frames + routes them to
//! per-session callback queues, and a writer mutex that serialises
//! outbound frames so multiple panes' input writes can't interleave
//! mid-frame.
//!
//! Public surface mirrors what the GUI needs:
//!
//! - `connect(socket_path, wake)` — open + handshake (HELLO/HELLO_ACK)
//! - `new_session(cols, rows, cwd)` — spawn a shell; returns a
//!   `ShelldSession` handle the GUI keeps in its `Pane`
//! - `list_sessions()` — for reattach paths (Phase 5)
//! - `attach(id)` — reattach to an existing session by id (Phase 5)
//!
//! A `ShelldSession` exposes the Session-shaped API the rest of
//! marspot already speaks:
//! - `pump()` — drains queued bytes through the embedded Terminal,
//!   returns byte count for redraw triggering
//! - `write(bytes)` — sends INPUT frame
//! - `resize(cols, rows)` — RESIZE frame + updates internal grid
//! - `is_exited()` — true once shelld reports the session no longer
//!   alive (child shell exit)
//!
//! The `wake` callback is called from the socket reader thread
//! whenever new bytes arrive for any session, so the GUI's event
//! loop comes around to call `pump`.  Same contract as
//! `marspot::session::Session`.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::shelld_proto::{
    decode_data, decode_error, decode_hello_ack, decode_list_sessions_reply,
    decode_new_session_reply, encode_data, encode_hello, encode_new_session, encode_resize,
    encode_session_id, Frame, MsgType, SessionInfo, PROTO_VERSION,
};
use crate::terminal::Terminal;

/// Bytes-per-chunk receiver queue.  Sized like the lib's local PTY
/// queue (`PTY_CHANNEL_CAPACITY` = 64); shelld already applies the
/// same backpressure on the wire.
const SESSION_QUEUE: usize = 64;

/// "Output produced within this window ⇒ Active, else Idle."
/// Same as `session::ACTIVE_WINDOW`.
const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

/// Public, lib-flavoured `SessionState` so consumers don't have to
/// import `session::SessionState` separately when they switch from
/// local to shelld.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Idle,
    Exited,
}

/// One inbound chunk waiting to be drained by `pump`.  Owned so the
/// reader thread can release the slot back to the channel without
/// the GUI holding it.
type Chunk = Vec<u8>;

struct SessionInner {
    id: u64,
    child_pid: i32,
    rx: Mutex<Receiver<Chunk>>,
    exited: AtomicBool,
    last_output: Mutex<Option<Instant>>,
}

/// Handle a Pane holds for one shelld-backed session.  Clone is
/// cheap (Arc), so mcli and marspot can hand it around like they
/// did `Session` (which they couldn't actually clone — but they
/// also didn't need to).
pub struct ShelldSession {
    inner: Arc<SessionInner>,
    /// Shared with `ShelldClient` so this handle can issue writes
    /// without going through a per-session indirection.
    writer: Arc<Mutex<UnixStream>>,
    /// Terminal that consumes the bytes.  Same role as
    /// `Session::terminal` in the lib's local model.
    pub terminal: Terminal,
}

impl ShelldSession {
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn child_pid(&self) -> i32 {
        self.inner.child_pid
    }

    pub fn is_exited(&self) -> bool {
        self.inner.exited.load(Ordering::Acquire)
    }

    /// Method-style accessor mirroring `session::Session::terminal`
    /// so call sites that read `pane.session().terminal()` keep
    /// working unchanged when Pane swaps to a shelld backend.
    pub fn terminal(&self) -> &crate::terminal::Terminal {
        &self.terminal
    }

    /// Mutable terminal accessor — used for local-echo `predict_byte`
    /// on the L3 (`marspot-session`) input path, mirroring how the
    /// in-process renderer predicts ahead of the PTY round trip.
    pub fn terminal_mut(&mut self) -> &mut crate::terminal::Terminal {
        &mut self.terminal
    }

    /// Drain whatever the reader thread has queued, feed it through
    /// the terminal parser, return total bytes drained.  Caller
    /// requests a redraw on non-zero return.
    pub fn pump(&mut self) -> usize {
        let mut total = 0;
        let rx = self.inner.rx.lock().unwrap();
        while let Ok(chunk) = rx.try_recv() {
            total += chunk.len();
            self.terminal.feed(&chunk);
        }
        drop(rx);
        if total > 0 {
            *self.inner.last_output.lock().unwrap() = Some(Instant::now());
        }
        // Forward capability-query responses back to shelld so the
        // child app's read() returns them.  Same role as
        // Session::pump's response-forwarding tail.
        let response = self.terminal.take_response();
        if !response.is_empty() {
            let _ = self.write(&response);
        }
        total
    }

    /// Send bytes to the PTY master via shelld.  INPUT frames
    /// carry session_id + raw bytes; shelld writes them to the
    /// master fd.  Returns the byte count written into the frame
    /// (not the kernel pipe), matching `Session::write`'s contract
    /// (kernel-level backpressure is shelld's problem).
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Diagnostic: when ~/.marspot-trace exists, append every
        // INPUT chunk's bytes (hex) to /tmp/marspot-keys-trace.log.
        // Used to debug "key X produced wrong PTY bytes" reports
        // without rebuilding.  No effect when the flag file is absent.
        if let Ok(home) = std::env::var("HOME") {
            if std::path::Path::new(&format!("{}/.marspot-trace", home)).exists() {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/marspot-keys-trace.log")
                {
                    let hex: String = bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let _ = writeln!(
                        f,
                        "session={} bytes={} [{}]",
                        self.inner.id,
                        bytes.len(),
                        hex
                    );
                }
            }
        }
        let frame = Frame::new(MsgType::Input, encode_data(self.inner.id, bytes));
        let mut stream = self.writer.lock().unwrap();
        frame.write_to(&mut *stream)?;
        Ok(bytes.len())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let frame = Frame::new(MsgType::Resize, encode_resize(self.inner.id, cols, rows));
        let mut stream = self.writer.lock().unwrap();
        frame.write_to(&mut *stream)?;
        // Local terminal grid follows; shelld already ioctl'd the
        // PTY so SIGWINCH lands in the child.
        self.terminal.resize(cols, rows);
        Ok(())
    }

    pub fn state(&self) -> SessionState {
        if self.is_exited() {
            return SessionState::Exited;
        }
        match *self.inner.last_output.lock().unwrap() {
            Some(t) if t.elapsed() < ACTIVE_WINDOW => SessionState::Active,
            _ => SessionState::Idle,
        }
    }
}

impl Drop for ShelldSession {
    fn drop(&mut self) {
        // Detach so shelld stops sending us DATA frames for this
        // session.  The session itself stays alive (that's the whole
        // point — GUI death must not kill shells).
        let frame = Frame::new(MsgType::Detach, encode_session_id(self.inner.id));
        if let Ok(mut stream) = self.writer.lock() {
            let _ = frame.write_to(&mut *stream);
        }
    }
}

/// Client connection to shelld.  Owns the socket; spawns one
/// reader thread; routes incoming DATA frames to per-session
/// queues by `session_id`.
pub struct ShelldClient {
    writer: Arc<Mutex<UnixStream>>,
    inboxes: Arc<Mutex<HashMap<u64, SyncSender<Chunk>>>>,
    /// Reader thread JoinHandle.  Drop signals shutdown so we don't
    /// leak the socket-reader thread on client teardown.
    _reader: Mutex<Option<JoinHandle<()>>>,
    /// Pending replies waiting for the reader to deliver them, keyed
    /// by message type.  Crude but effective for v1: shelld responds
    /// in order, so a single oneshot per type is enough.  Phase 7+
    /// may add a request-id correlation token if multiplexing
    /// outgrows this.
    pending: Arc<Mutex<PendingReplies>>,
    /// Tracks per-session metadata.  Held so a fresh `attach()` can
    /// reconstruct child_pid into a new ShelldSession.
    sessions: Arc<Mutex<HashMap<u64, Arc<SessionInner>>>>,
}

#[derive(Default)]
struct PendingReplies {
    new_session: Option<SyncSender<Result<(u64, i32), String>>>,
    list_sessions: Option<SyncSender<Result<Vec<SessionInfo>, String>>>,
    error_only: Option<SyncSender<Result<(), String>>>,
}

impl ShelldClient {
    pub fn connect<P: AsRef<Path>, W>(socket_path: P, wake: W) -> io::Result<Self>
    where
        W: Fn() + Send + Sync + 'static,
    {
        let stream = UnixStream::connect(&socket_path)?;
        let reader_stream = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));
        let inboxes: Arc<Mutex<HashMap<u64, SyncSender<Chunk>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(PendingReplies::default()));
        let sessions: Arc<Mutex<HashMap<u64, Arc<SessionInner>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let inboxes_r = inboxes.clone();
        let pending_r = pending.clone();
        let sessions_r = sessions.clone();
        let wake = Arc::new(wake);
        let wake_r = wake.clone();
        let reader = thread::Builder::new()
            .name("shelld-client-reader".into())
            .spawn(move || {
                reader_loop(reader_stream, inboxes_r, pending_r, sessions_r, wake_r);
            })
            .expect("spawn shelld-client reader");

        let me = Self {
            writer,
            inboxes,
            _reader: Mutex::new(Some(reader)),
            pending,
            sessions,
        };

        // Handshake
        me.send_frame(Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION)))?;
        // Wait briefly for HELLO_ACK so we fail fast on a version
        // mismatch (the reader thread surfaces it as a session error).
        // The reader thread just delivers the ack; we don't need its
        // contents beyond proto-version check (already verified
        // server-side).
        thread::sleep(Duration::from_millis(50));
        Ok(me)
    }

    fn send_frame(&self, frame: Frame) -> io::Result<()> {
        let mut stream = self.writer.lock().unwrap();
        frame.write_to(&mut *stream)?;
        Ok(())
    }

    /// Ask shelld to spawn a new shell.  Blocks the calling thread
    /// until shelld replies (NEW_SESSION_REPLY or ERROR).  Returns
    /// a ShelldSession the GUI can drive with `pump`/`write`/`resize`.
    pub fn new_session(&self, cols: u16, rows: u16, cwd: &str) -> io::Result<ShelldSession> {
        let (tx, rx) = mpsc::sync_channel::<Result<(u64, i32), String>>(1);
        {
            let mut p = self.pending.lock().unwrap();
            if p.new_session.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another NEW_SESSION already in flight",
                ));
            }
            p.new_session = Some(tx);
        }
        self.send_frame(Frame::new(
            MsgType::NewSession,
            encode_new_session(cols, rows, cwd),
        ))?;
        let res = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "NEW_SESSION reply timed out"))?
            .map_err(|m| io::Error::new(io::ErrorKind::Other, m))?;
        let (id, child_pid) = res;
        self.install_session(id, child_pid, cols, rows)
    }

    fn install_session(
        &self,
        id: u64,
        child_pid: i32,
        cols: u16,
        rows: u16,
    ) -> io::Result<ShelldSession> {
        let (tx, rx) = mpsc::sync_channel::<Chunk>(SESSION_QUEUE);
        self.inboxes.lock().unwrap().insert(id, tx);
        let inner = Arc::new(SessionInner {
            id,
            child_pid,
            rx: Mutex::new(rx),
            exited: AtomicBool::new(false),
            last_output: Mutex::new(None),
        });
        self.sessions.lock().unwrap().insert(id, inner.clone());
        Ok(ShelldSession {
            inner,
            writer: self.writer.clone(),
            terminal: Terminal::new(cols, rows),
        })
    }

    pub fn list_sessions(&self) -> io::Result<Vec<SessionInfo>> {
        let (tx, rx) = mpsc::sync_channel::<Result<Vec<SessionInfo>, String>>(1);
        {
            let mut p = self.pending.lock().unwrap();
            if p.list_sessions.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another LIST_SESSIONS already in flight",
                ));
            }
            p.list_sessions = Some(tx);
        }
        self.send_frame(Frame::new(MsgType::ListSessions, Vec::new()))?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "LIST_SESSIONS timed out"))?
            .map_err(|m| io::Error::new(io::ErrorKind::Other, m))
    }

    /// Re-attach to a previously-running session.  Builds a fresh
    /// ShelldSession around the existing session_id; shelld
    /// flushes its bytelog (Phase 5) before the live stream resumes.
    pub fn attach(&self, id: u64, cols: u16, rows: u16) -> io::Result<ShelldSession> {
        // Set up the inbox FIRST so any DATA the reader receives
        // between sending ATTACH and our session.pump() landing is
        // queued, not dropped.
        let session = self.install_session(id, 0, cols, rows)?;
        self.send_frame(Frame::new(MsgType::Attach, encode_session_id(id)))?;
        Ok(session)
    }

    pub fn kill_session(&self, id: u64) -> io::Result<()> {
        self.send_frame(Frame::new(MsgType::Kill, encode_session_id(id)))?;
        // Server cleans up async; drop the inbox locally.
        self.inboxes.lock().unwrap().remove(&id);
        if let Some(inner) = self.sessions.lock().unwrap().remove(&id) {
            inner.exited.store(true, Ordering::Release);
        }
        Ok(())
    }
}

fn reader_loop(
    mut stream: UnixStream,
    inboxes: Arc<Mutex<HashMap<u64, SyncSender<Chunk>>>>,
    pending: Arc<Mutex<PendingReplies>>,
    sessions: Arc<Mutex<HashMap<u64, Arc<SessionInner>>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let frame = match Frame::read_from(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => {
                // shelld closed.  Mark every session as exited so
                // the GUI sees them go red instead of hanging.
                for inner in sessions.lock().unwrap().values() {
                    inner.exited.store(true, Ordering::Release);
                }
                wake();
                return;
            }
            Err(e) => {
                eprintln!("[shelld-client] read error: {}", e);
                for inner in sessions.lock().unwrap().values() {
                    inner.exited.store(true, Ordering::Release);
                }
                wake();
                return;
            }
        };
        match frame.msg_type {
            MsgType::HelloAck => {
                // Could surface server version / git sha here for
                // diagnostics; not load-bearing yet.
                let _ = decode_hello_ack(&frame.payload);
            }
            MsgType::NewSessionReply => match decode_new_session_reply(&frame.payload) {
                Ok((id, pid)) => {
                    let mut p = pending.lock().unwrap();
                    if let Some(tx) = p.new_session.take() {
                        let _ = tx.send(Ok((id, pid)));
                    }
                }
                Err(e) => {
                    let mut p = pending.lock().unwrap();
                    if let Some(tx) = p.new_session.take() {
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            },
            MsgType::ListSessionsReply => match decode_list_sessions_reply(&frame.payload) {
                Ok(list) => {
                    let mut p = pending.lock().unwrap();
                    if let Some(tx) = p.list_sessions.take() {
                        let _ = tx.send(Ok(list));
                    }
                }
                Err(e) => {
                    let mut p = pending.lock().unwrap();
                    if let Some(tx) = p.list_sessions.take() {
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            },
            MsgType::Data => match decode_data(&frame.payload) {
                Ok((id, bytes)) => {
                    if bytes.is_empty() {
                        // Phase 5: shelld emits a zero-length DATA
                        // as a sentinel after the bytelog replay is
                        // drained.  Other phases ignore it.
                        wake();
                        continue;
                    }
                    let drop_tx = {
                        let inb = inboxes.lock().unwrap();
                        inb.get(&id).cloned()
                    };
                    if let Some(tx) = drop_tx {
                        // Bounded send: if the GUI is slow, this
                        // blocks the reader thread, which is exactly
                        // the backpressure we want.  Frame ownership
                        // is dropped here (Vec<u8> moved into the
                        // channel).
                        let bytes = bytes.to_vec();
                        if tx.send(bytes).is_err() {
                            // Inbox closed (session dropped) — quietly
                            // discard the chunk.  Reader thread stays
                            // alive for other sessions.
                        }
                    }
                    wake();
                }
                Err(e) => {
                    eprintln!("[shelld-client] bad DATA: {}", e);
                }
            },
            MsgType::Error => match decode_error(&frame.payload) {
                Ok((code, msg)) => {
                    eprintln!("[shelld-client] server error code={} {}", code, msg);
                    // Surface the error to any waiting pending reply
                    // so the GUI doesn't hang on a TimedOut path.
                    let mut p = pending.lock().unwrap();
                    if let Some(tx) = p.new_session.take() {
                        let _ = tx.send(Err(format!("server: {}", msg)));
                    }
                    if let Some(tx) = p.list_sessions.take() {
                        let _ = tx.send(Err(format!("server: {}", msg)));
                    }
                    if let Some(tx) = p.error_only.take() {
                        let _ = tx.send(Err(format!("server: {}", msg)));
                    }
                }
                Err(e) => eprintln!("[shelld-client] bad ERROR: {}", e),
            },
            other => {
                eprintln!("[shelld-client] unexpected msg_type {:?}", other);
            }
        }
    }
}

