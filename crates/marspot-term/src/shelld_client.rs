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
    decode_new_session_reply, decode_scrollback_page, decode_snapshot_payload, encode_attach,
    encode_data, encode_hello, encode_new_session, encode_resize, encode_session_id, Frame,
    MsgType, SessionInfo, PROTO_VERSION,
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

/// One inbound message waiting to be drained by `pump`.  Owned so
/// the reader thread can release the slot back to the channel without
/// the GUI holding it.
///
/// RFC-002: this used to be a bare `Vec<u8>` (raw PTY bytes).  After
/// the state-object-sync rewrite, ATTACH now responds with a
/// `StateSnapshot` frame instead of streaming the bytelog; the
/// reader routes that to the same per-session channel so `pump` can
/// apply it in-order against any live data already queued.
pub enum InboundMessage {
    /// Raw PTY bytes that the terminal parser feeds incrementally.
    /// Live-stream side after attach is complete.
    Data(Vec<u8>),
    /// Full terminal-state snapshot from shelld's per-session slot.
    /// Carried on ATTACH and on a future explicit refresh path.
    /// `pump` calls `terminal.apply_snapshot(&body)` and discards
    /// any earlier same-frame chunks (the snapshot is the new
    /// origin point).
    Snapshot {
        /// Echoed from shelld for forensic; the client doesn't need
        /// it for correctness because LWW is handled inside Terminal.
        generation: u64,
        body: Vec<u8>,
    },
    /// RFC-002 §8: reply to a `request_scrollback_page` call.  The
    /// page covers `[line_start, line_start + line_count)` rows of
    /// historic scrollback (oldest = `line_start = 0`).  Empty
    /// `line_count` = "no more history past this point".  The body
    /// is the encoded per-line cell payload; decode with
    /// `Terminal::decode_scrollback_page_body`.
    ScrollbackPage {
        line_start: u32,
        line_count: u32,
        body: Vec<u8>,
    },
}

type Chunk = InboundMessage;

struct SessionInner {
    id: u64,
    child_pid: i32,
    rx: Mutex<Receiver<Chunk>>,
    exited: AtomicBool,
    last_output: Mutex<Option<Instant>>,
    /// Latest cols/rows the client believes its terminal mirror is
    /// at.  Read by the reader_loop reconnect path so an auto-
    /// reattach after a transient disconnect still tells L4 the
    /// correct geometry to snapshot at.  Updated by
    /// `ShelldSession::resize`.
    last_dims: Mutex<(u16, u16)>,
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
    /// RFC-002 §8: `pump` drains `ScrollbackPage` frames out of the
    /// per-session channel into here so the caller (L3) can pull
    /// them on its own cadence — pages don't disturb the terminal
    /// state, they belong upstream of the publish path.
    pending_scrollback_pages: Vec<PendingPage>,
}

/// Decoded reply slot.  Body is left undecoded (raw cells) — the
/// caller (L3 publish-cache) controls when / if to decode.
pub struct PendingPage {
    pub line_start: u32,
    pub line_count: u32,
    pub body: Vec<u8>,
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
    ///
    /// RFC-002: handles both raw `Data` chunks and full `Snapshot`
    /// frames in arrival order.  A snapshot is the new origin point —
    /// it replaces grid + cursor + modes wholesale and resets the
    /// parser's transient state, so any queued same-frame `Data`
    /// chunks that landed *before* it (rare race against the SyncSender
    /// ordering) are still safe to apply afterward; the snapshot's
    /// generation has already accounted for everything up through
    /// its own bump.  Subsequent live `Data` chunks layer on top of
    /// the snapshot via the parser as usual.
    pub fn pump(&mut self) -> usize {
        let mut total = 0;
        let rx = self.inner.rx.lock().unwrap();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                InboundMessage::Data(bytes) => {
                    total += bytes.len();
                    self.terminal.feed(&bytes);
                }
                InboundMessage::Snapshot { generation, body } => {
                    if body.is_empty() {
                        // Empty body = "first attach, no snapshot
                        // exists yet".  Start from a fresh terminal —
                        // already constructed that way at install_session.
                        crate::lx_debug!(
                            "shelld_client.snapshot.empty_initial",
                            "no prior snapshot, start from empty terminal",
                            generation = generation
                        );
                    } else {
                        match self.terminal.apply_snapshot(&body) {
                            Ok(()) => {
                                crate::lx_event!(
                                    "ATTACH_SNAPSHOT_APPLIED",
                                    "RFC-002 client applied StateSnapshot",
                                    generation = generation,
                                    body_bytes = body.len()
                                );
                            }
                            Err(e) => {
                                crate::lx_error!(
                                    "shelld_client.snapshot.apply_failed",
                                    &format!("{e}"),
                                    generation = generation,
                                    body_bytes = body.len()
                                );
                                // Don't return — the terminal is
                                // unchanged; subsequent live data still
                                // flows.  User will see a stale grid
                                // until next snapshot, but no corruption.
                            }
                        }
                    }
                    // Snapshot itself isn't "PTY bytes"; don't count it
                    // toward `total`.  Caller uses `total > 0` as a
                    // "did anything happen?" signal but apply_snapshot
                    // changes the grid, so force a redraw poke by
                    // claiming 1 byte of progress.
                    total = total.max(1);
                }
                InboundMessage::ScrollbackPage { line_start, line_count, body } => {
                    // RFC-002 §8: stash for the L3 caller — pages don't
                    // change the live grid, so they don't contribute to
                    // `total` (no redraw poke needed; caller decides
                    // when to repaint after ingesting the page).
                    self.pending_scrollback_pages.push(PendingPage {
                        line_start,
                        line_count,
                        body,
                    });
                }
            }
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

    /// RFC-002 §8: ask shelld for `count` lines of historic scrollback
    /// starting at `line_start` (0 = oldest in L4's ring).  The reply
    /// is asynchronous — the next `pump()` drains a `ScrollbackPage`
    /// frame into `pending_scrollback_pages`; consumer reads via
    /// `take_pending_scrollback_pages()`.
    pub fn request_scrollback_page(
        &self,
        line_start: u32,
        count: u32,
    ) -> io::Result<()> {
        let payload = crate::shelld_proto::encode_get_scrollback_page(
            self.inner.id,
            line_start,
            count,
        );
        let frame = Frame::new(MsgType::GetScrollbackPage, payload);
        let mut stream = self.writer.lock().unwrap();
        frame.write_to(&mut *stream)?;
        Ok(())
    }

    /// Drain any scrollback pages collected since the last call.
    /// Empty Vec when none are pending.  Pages arrive out-of-band
    /// from terminal state, so callers can poll on whatever cadence
    /// fits (e.g. once per publish tick).
    pub fn take_pending_scrollback_pages(&mut self) -> Vec<PendingPage> {
        std::mem::take(&mut self.pending_scrollback_pages)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let frame = Frame::new(MsgType::Resize, encode_resize(self.inner.id, cols, rows));
        let mut stream = self.writer.lock().unwrap();
        frame.write_to(&mut *stream)?;
        // Local terminal grid follows; shelld already ioctl'd the
        // PTY so SIGWINCH lands in the child.
        self.terminal.resize(cols, rows);
        // Stamp so the reader_loop auto-reattach uses the live dims
        // instead of the NewSession-time ones.
        *self.inner.last_dims.lock().unwrap() = (cols, rows);
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
        let writer = Arc::new(Mutex::new(stream));
        let inboxes: Arc<Mutex<HashMap<u64, SyncSender<Chunk>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(PendingReplies::default()));
        let sessions: Arc<Mutex<HashMap<u64, Arc<SessionInner>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let writer_s = writer.clone();
        let inboxes_r = inboxes.clone();
        let pending_r = pending.clone();
        let sessions_r = sessions.clone();
        let wake = Arc::new(wake);
        let wake_r = wake.clone();
        let socket_path_buf: std::path::PathBuf = socket_path.as_ref().to_path_buf();
        let reader = thread::Builder::new()
            .name("shelld-client-reader".into())
            .spawn(move || {
                // The supervisor owns the read-fd lifecycle: it runs
                // `reader_loop` until EOF / error and on disconnect
                // attempts to reconnect to the same socket path with
                // exponential backoff (up to ~10 s total). On a
                // successful reconnect it re-handshakes and re-issues
                // ATTACH for every session id still in the client's
                // map — execv-driven shelld restarts preserve session
                // ids + PTYs, so the bytelog replay rebuilds terminal
                // state and the GUI never sees an "exited" blink.
                //
                // Only when the reconnect loop exhausts the backoff
                // budget do we fall through to marking sessions exited.
                supervisor_loop(
                    socket_path_buf,
                    writer_s,
                    inboxes_r,
                    pending_r,
                    sessions_r,
                    wake_r,
                );
            })
            .expect("spawn shelld-client supervisor");

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

    /// Create a session in shelld and return only its id — **without**
    /// installing a local subscription.  This is the L2-allocates /
    /// L3-attaches split for the per-session L3 model (target #4): L2
    /// owns session *assignment* (so N L3 children never race for the
    /// same session) but never drives the byte stream itself, so it
    /// holds no inbox/terminal for the session.  shelld keeps the
    /// session + bytelog alive with zero subscribers (GUI death must not
    /// kill shells); the L3 that L2 hands this id to attaches and
    /// replays the bytelog.
    ///
    /// shelld **auto-attaches the creating connection** on NEW_SESSION (so
    /// a normal `new_session` caller doesn't miss early output).  We don't
    /// want that here: L2 would then be subscribed to every session it
    /// allocates and shelld would broadcast every DATA chunk to L2 too —
    /// doubling shelld's broadcast load and flooding L2's reader with bytes
    /// it never reads (no inbox installed → dropped).  So we immediately
    /// DETACH; the L3's own attach + bytelog replay loses nothing.
    pub fn create_session(&self, cols: u16, rows: u16, cwd: &str) -> io::Result<u64> {
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
        let (id, _child_pid) = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "NEW_SESSION reply timed out"))?
            .map_err(|m| io::Error::new(io::ErrorKind::Other, m))?;
        // Drop the auto-attached subscription — L2 allocates, it doesn't read.
        self.send_frame(Frame::new(MsgType::Detach, encode_session_id(id)))?;
        Ok(id)
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
            last_dims: Mutex::new((cols, rows)),
        });
        self.sessions.lock().unwrap().insert(id, inner.clone());
        Ok(ShelldSession {
            inner,
            writer: self.writer.clone(),
            terminal: Terminal::new(cols, rows),
            pending_scrollback_pages: Vec::new(),
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
    /// ShelldSession around the existing session_id; shelld replies
    /// with a StateSnapshot at the negotiated cols/rows.
    ///
    /// RFC-002 §4 (step 8d): `cols`/`rows` are shipped in the ATTACH
    /// frame so L4 resizes its master Terminal to match before
    /// serialising the snapshot.  Without this, an L3 spawned by a
    /// fresh core (after install-local's UPDATE_SWAP) at a new
    /// window size would receive a snapshot at L4's NewSession-time
    /// dims and apply a grid that doesn't match what L2 expects to
    /// render — the install-blank-grid regression.
    pub fn attach(&self, id: u64, cols: u16, rows: u16) -> io::Result<ShelldSession> {
        // Set up the inbox FIRST so any DATA the reader receives
        // between sending ATTACH and our session.pump() landing is
        // queued, not dropped.
        let session = self.install_session(id, 0, cols, rows)?;
        self.send_frame(Frame::new(
            MsgType::Attach,
            encode_attach(id, cols, rows),
        ))?;
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

    /// Persist a display title against the given session at shelld.
    /// Fire-and-forget — shelld stores it and surfaces it on the
    /// next `list_sessions`.  Empty `title` clears the custom title.
    /// This is what makes user-set titles survive a dual-core
    /// silent swap: shelld outlives every core image.
    pub fn set_title(&self, id: u64, title: &str) -> io::Result<()> {
        self.send_frame(Frame::new(
            MsgType::SetTitle,
            crate::shelld_proto::encode_set_title(id, title),
        ))?;
        Ok(())
    }
}

/// Drives reconnect-on-EOF over the same socket path so an execv-style
/// daemon swap doesn't observable as "all sessions exited" to the GUI.
///
/// Lifecycle:
///   1. Clone a reader fd from the current writer, run `reader_loop`.
///   2. On reader_loop return (EOF / read error) acquire the writer mutex
///      and try to reconnect with exponential backoff (50 ms → 2 s,
///      capped, ~10 s total budget).
///   3. On success: swap the new stream into the writer, re-send HELLO,
///      re-send ATTACH for every session id the client currently knows
///      about. shelld's ATTACH path replays the bytelog, restoring
///      terminal state. Then loop back to step 1 with the new fd.
///   4. On exhaustion: mark every session as exited and exit the
///      supervisor — at that point the daemon is truly gone, not just
///      swapping images.
fn supervisor_loop(
    socket_path: std::path::PathBuf,
    writer: Arc<Mutex<UnixStream>>,
    inboxes: Arc<Mutex<HashMap<u64, SyncSender<Chunk>>>>,
    pending: Arc<Mutex<PendingReplies>>,
    sessions: Arc<Mutex<HashMap<u64, Arc<SessionInner>>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let reader_stream = match writer.lock().unwrap_or_else(|e| e.into_inner()).try_clone() {
            Ok(s) => s,
            Err(_) => {
                // Catastrophic — we can't even dup the stream. Give up.
                mark_all_exited(&sessions, &wake);
                return;
            }
        };
        reader_loop(
            reader_stream,
            inboxes.clone(),
            pending.clone(),
            sessions.clone(),
            wake.clone(),
        );

        // Reader returned → EOF or read error. Hold the writer lock for
        // the full reconnect+rehandshake window so any in-flight
        // `send_frame` blocks until we're back on a live stream rather
        // than writing into a closed fd.
        let mut w_guard = writer.lock().unwrap_or_else(|e| e.into_inner());
        let new_stream = match reconnect_with_backoff(&socket_path) {
            Some(s) => s,
            None => {
                drop(w_guard);
                mark_all_exited(&sessions, &wake);
                return;
            }
        };
        *w_guard = new_stream;
        // Re-handshake. A write failure here is treated like a fresh
        // disconnect — drop back into the reader_loop call at the top,
        // see EOF, and re-enter reconnect.
        let hello = Frame::new(MsgType::Hello, encode_hello(PROTO_VERSION));
        if hello.write_to(&mut *w_guard).is_err() {
            continue;
        }
        // Re-attach each session id the client thinks it still owns,
        // shipping the last-known cols/rows so L4 resizes its
        // master Terminal before serialising the new StateSnapshot.
        // Without this, an L3 that resized after attach would auto-
        // reattach and silently snap back to NewSession dims.
        let attach_set: Vec<(u64, u16, u16)> = {
            let s = sessions.lock().unwrap();
            s.iter()
                .map(|(id, inner)| {
                    let (c, r) = *inner.last_dims.lock().unwrap();
                    (*id, c, r)
                })
                .collect()
        };
        for (id, cols, rows) in &attach_set {
            let f = Frame::new(
                MsgType::Attach,
                encode_attach(*id, *cols, *rows),
            );
            if f.write_to(&mut *w_guard).is_err() {
                break;
            }
        }
        // Release the writer mutex so user-side send_frame calls can
        // resume.
        drop(w_guard);
        // Wake the GUI so its event loop comes around and pumps the
        // bytelog replay frames as soon as the reader thread starts
        // forwarding them.
        wake();
    }
}

fn reconnect_with_backoff(path: &std::path::Path) -> Option<UnixStream> {
    let mut backoff = Duration::from_millis(50);
    // Total budget: 50 + 100 + 200 + 400 + 800 + 1600 + 2000 + 2000 + 2000 ≈ 9.2 s.
    for _ in 0..9 {
        thread::sleep(backoff);
        if let Ok(s) = UnixStream::connect(path) {
            return Some(s);
        }
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
    None
}

fn mark_all_exited(
    sessions: &Arc<Mutex<HashMap<u64, Arc<SessionInner>>>>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    for inner in sessions.lock().unwrap().values() {
        inner.exited.store(true, Ordering::Release);
    }
    wake();
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
                // EOF — supervisor decides whether to reconnect or to
                // give up + mark sessions exited.
                let _ = (&sessions, &wake);
                return;
            }
            Err(_) => {
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
                        // RFC-002: zero-length DATA used to be the
                        // bytelog-replay-done sentinel.  After step 4,
                        // ATTACH no longer streams the bytelog at all,
                        // so an empty DATA frame should never appear.
                        // Wake conservatively (idempotent) and move on
                        // — strict drop would be brittle against any
                        // future server-side emitter.
                        wake();
                        continue;
                    }
                    let drop_tx = {
                        let inb = inboxes.lock().unwrap();
                        inb.get(&id).cloned()
                    };
                    if let Some(tx) = drop_tx {
                        // Bounded send: if the GUI is slow, this
                        // blocks the reader thread — exactly the
                        // backpressure we want, mirroring kernel
                        // pipe pushback to the child.
                        let bytes = bytes.to_vec();
                        if tx.send(InboundMessage::Data(bytes)).is_err() {
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
            MsgType::StateSnapshot => match decode_snapshot_payload(&frame.payload) {
                Ok((id, generation, body)) => {
                    let drop_tx = {
                        let inb = inboxes.lock().unwrap();
                        inb.get(&id).cloned()
                    };
                    if let Some(tx) = drop_tx {
                        // Snapshot replaces transient state — apply in
                        // arrival order against any queued Data.  Owned
                        // body so the channel doesn't borrow `frame`.
                        if tx
                            .send(InboundMessage::Snapshot {
                                generation,
                                body: body.to_vec(),
                            })
                            .is_err()
                        {
                            // Inbox closed.  Session caller will see
                            // the channel drop on their next pump.
                        }
                    }
                    wake();
                }
                Err(e) => {
                    eprintln!("[shelld-client] bad StateSnapshot: {}", e);
                }
            },
            MsgType::ScrollbackPage => match decode_scrollback_page(&frame.payload) {
                Ok((id, line_start, line_count, body)) => {
                    let drop_tx = {
                        let inb = inboxes.lock().unwrap();
                        inb.get(&id).cloned()
                    };
                    if let Some(tx) = drop_tx {
                        if tx
                            .send(InboundMessage::ScrollbackPage {
                                line_start,
                                line_count,
                                body: body.to_vec(),
                            })
                            .is_err()
                        {
                            // Inbox closed; drop silently.
                        }
                    }
                    wake();
                }
                Err(e) => {
                    eprintln!("[shelld-client] bad ScrollbackPage: {}", e);
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

