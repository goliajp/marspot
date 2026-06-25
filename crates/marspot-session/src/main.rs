//! marspot-session — the per-session L3 process (target #4).
//!
//! Runs ONE terminal session's parser/terminal half and nothing else:
//! it connects to shelld, attaches (or creates) a single session, and
//! pumps PTY bytes through the VT parser into a `Grid`.  It links only
//! `marspot-term` (the zero-GUI engine) — no Metal, AppKit, or
//! CoreText — so it floors at ~3–5 MB resident.  The renderer (L2,
//! `marspot-core`) will read this process's grid over shared memory and
//! composite it; that, plus the L2↔L3 control channel and per-session
//! silent update, land in later steps.  See `docs/per-session-l3.md`.
//!
//! Step 1 proved the floor (boot, attach shelld, pump bytes → grid).
//! Step 3a makes L3 a real session backend: it takes the grid-shm
//! region L2 created (inherited as `MARSPOT_SHM_FD`) as the writer, and
//! reads keystrokes L2 forwards over the control socket (fd 3,
//! `MARSPOT_SHELL_CONTROL_FD`) — encoding them itself with its own
//! terminal mode flags, writing to the PTY, and local-echoing ahead of
//! the round trip. Still event-driven: it blocks on a unified event
//! channel fed by both the shelld wake and the control reader, so idle
//! CPU is ~0. With neither env var set it stays fully standalone
//! (self-creates the region, no input source) for the dev/test path.

mod local_session;
mod uds_server;

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use marspot_term::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot_term::grid_shm::{
    GridShmWriter, ENV_SHM_FD, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE,
    FLAG_MOUSE_SGR, FLAG_MOUSE_TRACKING,
};
use marspot_term::input_core::{MarspotKeyEvent, Modifiers};
use marspot_term::render::grid_selection_text;
use marspot_term::scrollback_search::{
    spawn_search_merged, LiveGridSnapshot, SearchOpts, SearchWorker,
};
use marspot_term::shell_proto::{
    decode_get_selection_text, decode_grid_resize, decode_grid_scroll, decode_key_event,
    decode_paste, decode_search_cancel, decode_search_scrollback,
    encode_search_results, encode_selection_text, wire_to_event, Frame, MsgType,
    WireSearchHit, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
};
use marspot_term::session_state::{PendingPage, SessionState};

use local_session::LocalSession;

/// RFC-003 step 1c. Two session backends coexist:
///
/// * `Shelld` — the L4-mediated path. PTY + bytelog live in the
///   `marspot-shelld` daemon; this L3 talks to it over a unix socket
///   and consumes a snapshot/byte stream.
/// * `Local`  — the L3-owns-PTY path. PTY + bytelog live right here.
///   No L4 dependency.
///
/// Both expose the same surface to the main loop so the only
/// difference user-facing is the one env-var-gated dispatch in
/// `main()`. Phase 6 of RFC-003 deletes the `Shelld` variant once
/// L2 has fully switched over.
/// RFC-003 Phase 6: only the L3-owns-PTY path remains; SessionImpl
/// is a single-variant alias kept for code-shape continuity.
enum SessionImpl {
    Local(LocalSession),
}

impl SessionImpl {
    fn id(&self) -> u64 {
        match self {
            Self::Local(s) => s.id(),
        }
    }
    fn child_pid(&self) -> i32 {
        match self {
            Self::Local(s) => s.child_pid(),
        }
    }
    fn is_exited(&self) -> bool {
        match self {
            Self::Local(s) => s.is_exited(),
        }
    }
    fn state(&self) -> SessionState {
        match self {
            Self::Local(s) => s.state(),
        }
    }
    fn terminal(&self) -> &marspot_term::terminal::Terminal {
        match self {
            Self::Local(s) => s.terminal(),
        }
    }
    fn terminal_mut(&mut self) -> &mut marspot_term::terminal::Terminal {
        match self {
            Self::Local(s) => s.terminal_mut(),
        }
    }
    fn pump(&mut self) -> usize {
        match self {
            Self::Local(s) => s.pump(),
        }
    }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(s) => s.write(bytes),
        }
    }
    fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        match self {
            Self::Local(s) => s.resize(cols, rows),
        }
    }
    fn request_scrollback_page(
        &mut self,
        line_start: u32,
        line_count: u32,
    ) -> std::io::Result<()> {
        match self {
            Self::Local(s) => s.request_scrollback_page(line_start, line_count),
        }
    }
    fn take_pending_scrollback_pages(&mut self) -> Vec<PendingPage> {
        match self {
            Self::Local(s) => s.take_pending_scrollback_pages(),
        }
    }
}

/// What wakes the L3 main loop. Both arms arrive on one channel so the
/// loop blocks in a single place (idle CPU ~0): the shelld reader thread
/// sends `Wake` when the PTY has bytes/EOF, the control reader sends
/// `Key` when L2 forwards a keystroke.
enum SessionEvent {
    Wake,
    Key(MarspotKeyEvent, Modifiers),
    /// L2 forwarded a cell-grid resize (window/layout changed). L3 resizes
    /// its Terminal + ioctl's the PTY (via shelld) + reflows, then
    /// republishes at the new dims into the same (capacity-mapped) region.
    Resize(u16, u16),
    /// L2 forwarded a scrollback view offset (rows up from live). L3 owns
    /// the scrollback, so it publishes that window; L2 renders the mirror
    /// as-is (it can't scroll its window-only mirror itself).
    Scroll(u16),
    /// L2 wants the clipboard text under a selection (Cmd-C). L3 owns the
    /// grid + scrollback, so it serialises the text and replies with a
    /// `SelectionText` frame. `(seq, anchor, focus, blockwise)`; `seq` is
    /// echoed back so a late reply can't alias a newer request.
    GetSelection(u32, (u16, u32), (u16, u32), bool),
    /// L2 resolved the macOS pasteboard (Cmd-V) and forwarded the text —
    /// the GUI-free L3 can't read the pasteboard itself. We bracketed-wrap
    /// it (per our terminal's mode) and write it to the PTY.
    Paste(String),
    /// cc plugin asked to push raw bytes straight into the PTY (no
    /// bracketed-paste wrap, no key encoding).  Used by the profile-
    /// cycle state machine to send `claude5 --resume <uuid>\r`.
    InjectInput(Vec<u8>),
    /// The control socket to L2/core hit EOF — our core is gone (crash,
    /// hang-kill, or clean exit). A freshly-booted core re-attaches the
    /// shelld session (PTY + bytelog survive *in shelld*, kept alive with
    /// zero subscribers) and spawns a brand-new L3 for it, so this
    /// orphaned engine has no reader left: it must exit, not linger.
    /// Lingering leaks one process + shm region per core restart, which
    /// violates the "cannot get slower the longer it runs" invariant.
    /// Standalone sessions have no control socket, so this never fires
    /// there.  RFC-003 §6 Amendment 11 debug-3: carries the client
    /// generation that owned the dying reader.  Main ignores a
    /// CoreGone whose generation is older than the current `poke` —
    /// otherwise an OLD L2's socket EOF, arriving AFTER a NEW L2 has
    /// already reattached, would null out the poke we just adopted.
    CoreGone(u64),
    /// RFC-003 step 2e: a UDS client finished its Hello handshake and
    /// is now the L3's control client.  Carries the validated stream
    /// so main can spawn a reader on it and adopt the write half as
    /// `poke`.  In owns-pty mode this replaces the inherited-fd
    /// control socket — owns-pty L3s have no fd 3 to read from.
    NewClient(UnixStream),
    /// RFC-003 §6 Amendment 15 — SIGTERM received.  L1 / L2 told us
    /// to retire (silent upgrade or user-driven marspot quit).  Main
    /// loop persists Terminal state to `state.bin`, then `exit(0)` —
    /// `process::exit` does NOT unwind, so `Pty::Drop` never runs and
    /// the shell is NOT SIGHUP'd.  L1's fd-vault still holds a duped
    /// PTY master fd (for the silent-upgrade replacement to withdraw),
    /// or the user-quit path will close that vault entry too, letting
    /// the shell exit on its own.
    ShutdownRequested,
    /// B3 — L2 asked for a scrollback search.  Main loop snapshots the
    /// File-backed scrollback off the live grid, cancels any in-flight
    /// worker (last-write-wins, D15), and spawns a new
    /// `SearchWorker`.  Worker callback re-enters the channel as
    /// `SearchHitsReady` so the wire emit happens on the main thread
    /// (single-writer to `poke`).
    SearchRequest {
        query_id: u32,
        case_sensitive: bool,
        max_total: u32,
        query: String,
    },
    /// B3 — L2 asked to cancel the in-flight query.  Drops the worker
    /// (sets its cancel flag); a late `SearchHitsReady` from the
    /// cancelled worker is dropped by the qid-match check in the main
    /// loop.
    SearchCancelRequested(u32),
    /// B3 — worker thread finished (or was cancelled cleanly) and
    /// posted its batch.  Main loop encodes a `SearchResults` frame +
    /// writes to `poke` IFF the qid matches the still-current worker;
    /// stale batches from a worker that was cancelled by a newer
    /// `SearchRequest` are silently dropped.
    SearchHitsReady {
        query_id: u32,
        hits: Vec<WireSearchHit>,
        has_more: bool,
        total_seen: u32,
    },
}

/// RFC-003 §6 Amendment 15 — SIGTERM self-pipe write end.  -1 until
/// the handler is installed.
static SIGTERM_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn l3_sigterm_handler(_: libc::c_int) {
    let fd = SIGTERM_PIPE_W.load(AtomicOrdering::Relaxed);
    if fd >= 0 {
        let buf: [u8; 1] = [1];
        unsafe { libc::write(fd, buf.as_ptr() as *const _, 1); }
    }
}

/// Install a SIGTERM handler that posts `SessionEvent::ShutdownRequested`
/// onto the main loop via a self-pipe.  Async-signal-safe write is the
/// only thing the handler does; the watcher thread translates each
/// wake byte into a regular SessionEvent so the loop processes it in
/// its normal drain.
fn install_sigterm_handler(ev_tx: Sender<SessionEvent>) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (r_fd, w_fd) = (fds[0], fds[1]);
    for fd in [r_fd, w_fd] {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
    SIGTERM_PIPE_W.store(w_fd, AtomicOrdering::Relaxed);

    std::thread::Builder::new()
        .name("l3-sigterm-watcher".to_string())
        .spawn(move || {
            let mut buf = [0u8; 1];
            loop {
                let n = unsafe { libc::read(r_fd, buf.as_mut_ptr() as *mut _, 1) };
                if n <= 0 { break; }
                if ev_tx.send(SessionEvent::ShutdownRequested).is_err() {
                    break;
                }
            }
        })?;

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = l3_sigterm_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn session_state_bin_path(id: u64) -> std::path::PathBuf {
    marspot_term::session_registry::session_dir(id).join("state.bin")
}

/// F2+1 — read-only mmap of a file as a byte slice.  Used for the
/// post-execv `state.bin` restore: `std::fs::read` puts the entire
/// snapshot body (~23 MB for a busy claudecode pane) onto the heap as
/// one `Vec<u8>`; when that Vec drops after `apply_snapshot` consumes
/// it, libmalloc retains the freed pages in the `MALLOC_LARGE (empty)`
/// bucket indefinitely (no orthodox way to reclaim — `pressure_relief`
/// is advisory and observed not to release these on macOS 26.5).
///
/// `mmap(PROT_READ, MAP_PRIVATE)` instead: pages are file-backed and
/// page-cache-managed by the kernel.  The slice we hand to
/// `apply_snapshot` is read-only; the decoder copies the bits it
/// keeps (Grid cells, scrollback rows, etc.) into its own buffers and
/// never retains the slice past return.  `Drop` munmaps and the
/// kernel reclaims pages on demand — they NEVER land in the malloc
/// empty-bucket footprint.  Direct 23 MB cut to the L3 physical
/// footprint per pane × 9 panes = ~210 MB fleet savings.
///
/// SAFETY contract:
/// - The `&[u8]` returned by `as_slice` is valid only for the
///   lifetime of the `MappedFile`.  Any consumer that wants to keep
///   bytes past Drop must copy them.
/// - The underlying file MUST NOT be truncated or written to during
///   the mapping's lifetime.  We mmap state.bin and unlink it after
///   apply_snapshot — that's safe because Unix unlink only removes
///   the directory entry; the inode stays alive until the last
///   mapping is dropped.
struct MappedFile {
    ptr: *const u8,
    len: usize,
}

impl MappedFile {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(MappedFile { ptr: std::ptr::null(), len: 0 });
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // Hint the kernel: we'll read this once sequentially.  Lets
        // the page cache evict our pages aggressively after read.
        unsafe {
            let _ = libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
        }
        Ok(MappedFile { ptr: ptr as *const u8, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe {
                libc::munmap(self.ptr as *mut _, self.len);
            }
        }
    }
}

/// RFC-003 §6 Amendment 16 — L3 self-execv handoff (L4 shelld model).
///
/// On SIGTERM, the L3 looks at the `current/marspot-session` binary
/// next door.  If its MARSPOT_FP fingerprint differs from this
/// process's rodata fingerprint, the L3 stages an execv handoff:
///
///   1. clear CLOEXEC on PTY master fd + UDS listener fd
///   2. write Terminal snapshot to `sessions/<id>/state.bin`
///   3. write manifest TSV to /tmp keyed by pid
///   4. set env sentinel `MARSPOT_L3_HANDOFF_MANIFEST=<path>`
///   5. `execv("current/marspot-session", argv)`
///
/// PID is preserved across execv; the master fd / listener fd / shell
/// child / PTY tty session are all unchanged.  The new image detects
/// the env sentinel at boot, reads the manifest, and rebuilds
/// LocalSession + SessionListener via `from_handoff` instead of
/// forkpty + bind.  L2 sees a brief read pause on its control
/// channel while the new image rebinds its reader; keystrokes resume
/// transparently.
///
/// Fingerprint-match (or missing `current/`) → fall through to the
/// user-quit cleanup path (state.bin + clean exit, shell SIGHUPs).
const ENV_HANDOFF_MANIFEST: &str = "MARSPOT_L3_HANDOFF_MANIFEST";
// Manifest version 2 (2026-06-17): added optional `control_stream_fd`
// so the L3 self-execv can carry the adopted L2↔L3 control UnixStream
// across the image swap.  Without it the stream's fd kept its default
// CLOEXEC=1, closed across execv, and L2 saw EOF + had no auto-reconnect
// → user couldn't type for many minutes after every install.
// Forward-compat: a v1 manifest is still accepted (`control_stream_fd`
// defaults to -1, meaning "no inherited stream — wait for NewClient").
const HANDOFF_MANIFEST_VERSION: u32 = 2;
const HANDOFF_MANIFEST_MIN_COMPAT: u32 = 1;

struct L3Handoff {
    master_fd: RawFd,
    child_pid: i32,
    listen_fd: RawFd,
    cols: u16,
    rows: u16,
    /// Adopted L2↔L3 control UnixStream raw fd, or `-1` if there was
    /// no live control stream when the execv handoff fired (which
    /// happens when L2 is mid-swap and the previous client EOF'd
    /// before NewClient adopted a new one).
    control_stream_fd: RawFd,
}

fn handoff_manifest_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/marspot-session-handoff.{}.tsv",
        std::process::id()
    ))
}

fn write_handoff_manifest(h: &L3Handoff, path: &std::path::Path) -> std::io::Result<()> {
    let body = format!(
        "version={}\nmaster_fd={}\nchild_pid={}\nlisten_fd={}\ncols={}\nrows={}\ncontrol_stream_fd={}\n",
        HANDOFF_MANIFEST_VERSION,
        h.master_fd, h.child_pid, h.listen_fd, h.cols, h.rows,
        h.control_stream_fd,
    );
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_handoff_manifest(path: &std::path::Path) -> std::io::Result<L3Handoff> {
    let body = std::fs::read_to_string(path)?;
    let mut fields = std::collections::HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let inv = |k: &str| std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("manifest bad/missing {k}"),
    );
    let version: u32 = fields.get("version").ok_or_else(|| inv("version"))?
        .parse().map_err(|_| inv("version"))?;
    if version < HANDOFF_MANIFEST_MIN_COMPAT || version > HANDOFF_MANIFEST_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "manifest version {version} not in [{HANDOFF_MANIFEST_MIN_COMPAT}, {HANDOFF_MANIFEST_VERSION}]"
            ),
        ));
    }
    // v1 lacked control_stream_fd — default to -1 (no inherited stream).
    let control_stream_fd: RawFd = fields
        .get("control_stream_fd")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    Ok(L3Handoff {
        master_fd: fields.get("master_fd").ok_or_else(|| inv("master_fd"))?
            .parse().map_err(|_| inv("master_fd"))?,
        child_pid: fields.get("child_pid").ok_or_else(|| inv("child_pid"))?
            .parse().map_err(|_| inv("child_pid"))?,
        listen_fd: fields.get("listen_fd").ok_or_else(|| inv("listen_fd"))?
            .parse().map_err(|_| inv("listen_fd"))?,
        cols: fields.get("cols").ok_or_else(|| inv("cols"))?
            .parse().map_err(|_| inv("cols"))?,
        rows: fields.get("rows").ok_or_else(|| inv("rows"))?
            .parse().map_err(|_| inv("rows"))?,
        control_stream_fd,
    })
}

fn try_resume_handoff() -> Option<L3Handoff> {
    let path = std::env::var(ENV_HANDOFF_MANIFEST).ok()?;
    unsafe { std::env::remove_var(ENV_HANDOFF_MANIFEST); }
    let path = std::path::PathBuf::from(path);
    match read_handoff_manifest(&path) {
        Ok(h) => {
            let _ = std::fs::remove_file(&path);
            Some(h)
        }
        Err(e) => {
            lx_warn!(
                "l3.handoff.manifest_read_failed",
                &format!("{e}; cold start"),
                path = path.display()
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Read the MARSPOT_FP=<sha>|<ts>|END rodata marker out of a binary.
/// Used by the SIGTERM handler to decide between execv-handoff and
/// user-quit cleanup.
fn read_binary_fingerprint(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let needle = b"MARSPOT_FP=";
    let start = buf.windows(needle.len()).position(|w| w == needle)?;
    let tail = &buf[start..];
    let end_off = tail.windows(4).position(|w| w == b"|END")?;
    std::str::from_utf8(&tail[..end_off]).ok().map(|s| s.to_string())
}

fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 { return Err(std::io::Error::last_os_error()); }
        if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Stage the execv handoff and replace this process's image with
/// `current/marspot-session`.  Returns Err only if we couldn't even
/// reach execv — on success the function does not return.
fn do_l3_execv_swap(
    id: u64,
    local: local_session::LocalSession,
    mut listener: uds_server::SessionListener,
    poke: Option<UnixStream>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let (master_fd, child_pid, terminal, cols, rows) = local.extract_for_handoff();

    let body = terminal.serialize_snapshot();
    let state_path = session_state_bin_path(id);
    let _ = std::fs::write(&state_path, &body);
    // F3+10 — flush the FileScrollback BufWriter tail to the kernel
    // page cache before execv.  Drop won't run on `libc::execv` so
    // anything sitting in the writer's 64 KB user-space buffer would
    // otherwise be lost to the next L3.  Snapshot v3 self-describing
    // index dedup is the safety net (it'll refill any remaining gap
    // from its RAM-ring section), but flushing keeps disk + RAM in
    // sync so the next L3's cold reads hit complete data immediately.
    terminal.grid().scrollback_flush_for_handoff();

    clear_cloexec(master_fd)?;
    let listen_fd = listener.prepare_for_execv()?;

    // Carry the adopted control stream across execv too, so L2's
    // reader doesn't see EOF.  `into_raw_fd` so the OwnedFd doesn't
    // close on drop after we've cleared CLOEXEC on it.
    let control_stream_fd: RawFd = match poke {
        Some(stream) => {
            use std::os::fd::IntoRawFd;
            let fd = stream.into_raw_fd();
            clear_cloexec(fd)?;
            fd
        }
        None => -1,
    };

    let manifest_path = handoff_manifest_path();
    let handoff = L3Handoff {
        master_fd, child_pid, listen_fd, cols, rows, control_stream_fd,
    };
    write_handoff_manifest(&handoff, &manifest_path)?;

    let target = marspot_term::binary_tree::BinaryTree::default_for("marspot-session")?.current();
    let target_c = std::ffi::CString::new(target.to_string_lossy().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in target"))?;
    let argv: [*const libc::c_char; 2] = [target_c.as_ptr(), std::ptr::null()];

    unsafe { std::env::set_var(ENV_HANDOFF_MANIFEST, &manifest_path); }

    lx_event!(
        "L3_EXECV_INVOKE",
        "execv into new marspot-session image",
        session_id = id,
        target = target.display(),
        manifest = manifest_path.display(),
        master_fd = master_fd,
        listen_fd = listen_fd,
        control_stream_fd = control_stream_fd,
        child_pid = child_pid,
        elapsed_us = started.elapsed().as_micros()
    );

    // Drop listener so suppress_drop runs (mem::forget keeps the
    // listener fd open for the new image).
    std::mem::drop(listener);

    unsafe { libc::execv(target_c.as_ptr(), argv.as_ptr()); }
    let err = std::io::Error::last_os_error();
    lx_error!("l3.execv.failed", &format!("{err}"), target = target.display());
    unsafe { std::env::remove_var(ENV_HANDOFF_MANIFEST); }
    let _ = std::fs::remove_file(&manifest_path);
    Err(err)
}

/// On SIGTERM, decide whether to execv (binary update) or clean-exit
/// (user quit / non-update SIGTERM).  Returns true when caller should
/// proceed with execv via `do_l3_execv_swap`.
fn should_execv_on_sigterm() -> bool {
    let Ok(tree) = marspot_term::binary_tree::BinaryTree::default_for("marspot-session") else {
        return false;
    };
    let current = tree.current();
    if !current.exists() {
        return false;
    }
    let Some(target_fp) = read_binary_fingerprint(&current) else {
        return false;
    };
    let self_fp = marspot_term::MARSPOT_FP_TERM;
    target_fp != self_fp
}

/// Placeholder geometry until L2 drives a real resize (later step).
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

fn state_str(s: SessionState) -> &'static str {
    match s {
        SessionState::Active => "active",
        SessionState::Idle => "idle",
        SessionState::Exited => "exited",
    }
}

/// Publish the session's grid window at `view_offset` (rows up from the
/// live tail, clamped to the available scrollback) + cursor/mode flags
/// into the shared framebuffer for L2 to render.  Returns `true` when
/// the publish actually wrote a new snapshot (i.e. the grid+cursor+
/// flags differ from the previous publish).  The caller uses the bool
/// to decide whether to send a GridReady poke — skipping the poke on
/// a content-identical publish is what keeps idle L2 asleep when a TUI
/// pumps the PTY at its own redraw cadence without moving any cell.
fn publish(shm: &mut GridShmWriter, session: &SessionImpl, view_offset: u16) -> bool {
    let term = session.terminal();
    let mut flags = 0u32;
    if term.cursor_visible() {
        flags |= FLAG_CURSOR_VISIBLE;
    }
    if term.cursor_key_application_mode() {
        flags |= FLAG_APP_CURSOR_KEYS;
    }
    if term.bracketed_paste_mode() {
        flags |= FLAG_BRACKETED_PASTE;
    }
    if term.mouse_tracking_mode() != marspot_term::terminal::MouseTrackingMode::Off {
        flags |= FLAG_MOUSE_TRACKING;
    }
    if term.mouse_sgr_encoding() {
        flags |= FLAG_MOUSE_SGR;
    }
    // Clamp here too: L2 clamps against the scrollback_len it last saw, but
    // scrollback can shrink (alt-screen enter / reset) between L2's request
    // and this publish, so never hand the writer an out-of-range offset.
    let off = view_offset.min(term.grid().scrollback_len() as u16);
    shm.publish_if_changed(term.grid(), off, flags)
}

/// Encode one forwarded keystroke with L3's own terminal mode flags,
/// write it to the PTY, and local-echo printable bytes ahead of the
/// round trip. Returns whether the echo painted the grid (so the caller
/// republishes even when no PTY bytes pumped this tick).
///
/// Clipboard reads resolve to `None` here: Cmd-V paste is forwarded by
/// L2 as already-resolved text in a later step, not pulled from the
/// pasteboard by the GUI-free L3.
fn handle_key(session: &mut SessionImpl, event: MarspotKeyEvent, mods: Modifiers) -> bool {
    let (app_mode, bracketed) = {
        let t = session.terminal();
        (t.cursor_key_application_mode(), t.bracketed_paste_mode())
    };
    let Some(bytes) =
        marspot_term::input_core::key_event_to_bytes(&event, mods, app_mode, bracketed, || None)
    else {
        return false;
    };
    let _ = session.write(&bytes);
    // Local-echo predict — but ONLY for plain printable runs.  An
    // escape sequence (ESC-leading) is a control code; the first
    // byte 0x1b isn't printable so predict_byte rejects it, but the
    // sequence's tail (`[`, `C`, …) is ASCII printable and would be
    // painted literally into the grid, producing the `[C[C[C` smear
    // the user reported every time they pressed an arrow key.  Just
    // skip the whole run when it starts with ESC.
    let mut predicted = false;
    if !bytes.starts_with(&[0x1b]) {
        for &b in bytes.as_ref() {
            if session.terminal_mut().predict_byte(b) {
                predicted = true;
            }
        }
    }
    predicted
}

/// Write pasted text (already resolved from the macOS pasteboard by L2) to
/// the PTY.  Wrap in bracketed-paste markers iff this terminal turned the
/// mode on (DECSET 2004) so the receiving app treats it as a single paste
/// rather than typed input.  No local predict — the PTY echo round-trips
/// back through the normal pump.
fn handle_paste(session: &mut SessionImpl, text: &str) {
    let bracketed = session.terminal().bracketed_paste_mode();
    let mut bytes: Vec<u8> = Vec::with_capacity(text.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    let _ = session.write(&bytes);
}

/// If L2 handed us a control socket (fd `MARSPOT_SHELL_CONTROL_FD`,
/// default 3), spawn a reader thread that decodes forwarded `KeyEvent`
/// frames and feeds them to the main loop, and return a write half so
/// the loop can poke L2 with `GridReady` after each publish. EOF / error
/// on the read side means L2/core is gone, so we signal the main loop to
/// exit (`CoreGone`): the surviving shelld session is re-attached by the
/// next core, so an orphaned engine would only leak (see `CoreGone`).
/// No env var → standalone, no input source, no writer.
fn setup_control_socket(tx: Sender<SessionEvent>) -> Option<UnixStream> {
    let fd: RawFd = match std::env::var(ENV_CONTROL_FD) {
        Ok(s) => match s.parse() {
            Ok(fd) => fd,
            Err(_) => {
                lx_warn!(
                    "session.control_fd.parse_failed",
                    "bad control fd env; ignoring",
                    env_var = ENV_CONTROL_FD,
                    value = s
                );
                return None;
            }
        },
        Err(_) => {
            let _ = DEFAULT_CONTROL_FD; // standalone: no control socket
            return None;
        }
    };
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            lx_error!(
                "session.control_socket.try_clone_failed",
                &format!("{e}"),
                fd = fd
            );
            return None;
        }
    };
    lx_info!("session.control_socket.attached", "control socket fd inherited", fd = fd);
    // The inherited-fd path runs at generation 0; if it dies, that's a
    // genuine "no client left" (not a stale-reader race).
    spawn_control_reader(stream, tx, 0);
    Some(writer)
}

/// RFC-003 step 2e — frame-dispatch loop for the L2↔L3 control socket.
/// Shared by both the inherited-fd path (`setup_control_socket`) and
/// the UDS accept path (`uds_server::handshake`) so both ends speak
/// the same protocol and unknown-frame handling lives in exactly one
/// place.  `generation` tags the CoreGone emitted on EOF so a stale
/// reader (from a swapped-out L2 socket) can't null the current poke.
fn spawn_control_reader(mut reader: UnixStream, tx: Sender<SessionEvent>, generation: u64) {
    std::thread::spawn(move || loop {
        match Frame::read_from(&mut reader) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::KeyEvent => {
                    if let Ok(w) = decode_key_event(&f.payload) {
                        let (e, m) = wire_to_event(w);
                        if tx.send(SessionEvent::Key(e, m)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GridResize => {
                    if let Ok((cols, rows)) = decode_grid_resize(&f.payload) {
                        if tx.send(SessionEvent::Resize(cols, rows)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GridScroll => {
                    if let Ok(off) = decode_grid_scroll(&f.payload) {
                        if tx.send(SessionEvent::Scroll(off)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GetSelectionText => {
                    if let Ok((seq, a, fo, bw)) = decode_get_selection_text(&f.payload) {
                        if tx.send(SessionEvent::GetSelection(seq, a, fo, bw)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::Paste => {
                    if let Ok(text) = decode_paste(&f.payload) {
                        if tx.send(SessionEvent::Paste(text)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::InjectInput => {
                    if let Ok((_sid, bytes)) =
                        marspot_term::shell_proto::decode_inject_input(&f.payload)
                    {
                        if tx.send(SessionEvent::InjectInput(bytes)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::SearchScrollback => {
                    if let Ok((query_id, case_sensitive, max_total, query)) =
                        decode_search_scrollback(&f.payload)
                    {
                        if tx
                            .send(SessionEvent::SearchRequest {
                                query_id,
                                case_sensitive,
                                max_total,
                                query,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                MsgType::SearchCancel => {
                    if let Ok(query_id) = decode_search_cancel(&f.payload) {
                        if tx
                            .send(SessionEvent::SearchCancelRequested(query_id))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                _ => {}
            },
            Ok(None) | Err(_) => {
                let _ = tx.send(SessionEvent::CoreGone(generation));
                break;
            }
        }
    });
}

/// Publish the grid and, if connected to L2, poke it so it re-reads the
/// shm — keeps L2 event-driven instead of polling per frame.
fn publish_and_poke(
    shm: &mut GridShmWriter,
    session: &mut SessionImpl,
    view_offset: u16,
    mut poke: Option<&mut UnixStream>,
) {
    let changed = publish(shm, session, view_offset);
    // F3+3.6 — OSC 7 push-based cwd publish removed; L2 now pull-
    // fetches cwd via `proc_pidinfo` when the user opens LayoutModal.
    let _ = &mut poke;
    // Dedup: a content-identical publish doesn't wake L2.  Without
    // this, busy TUIs (claudecode, htop, vim cursor) emit redraw
    // bytes that produced bit-identical shm snapshots, and each one
    // woke L2 for a full re-render — measured ~30 L3Ready/s at idle
    // before the gate.  When the grid genuinely changed we still
    // poke once, same as before.
    if !changed {
        return;
    }
    if let Some(w) = poke {
        // Best-effort: a dead socket just means L2 went away; the next
        // read EOF tears the reader down and the session keeps running.
        let _ = Frame::new(MsgType::GridReady, Vec::new()).write_to(w);
    }
}

/// Bring up the grid framebuffer and report its geometry. When L2 owns
/// the region (`MARSPOT_SHM_FD` set) we take the writer role on the
/// inherited fd and adopt the region's dims; standalone we self-create
/// at the placeholder geometry. The returned `(cols, rows)` is what the
/// session must be sized to so its published grid fits the region.
fn setup_shm() -> (GridShmWriter, u16, u16) {
    match std::env::var(ENV_SHM_FD) {
        Ok(s) => {
            let fd: RawFd = s.parse().unwrap_or_else(|_| {
                lx_error!(
                    "session.shm_fd.parse_failed",
                    "bad shm fd env",
                    env_var = ENV_SHM_FD,
                    value = s
                );
                std::process::exit(1);
            });
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let w = GridShmWriter::from_fd(owned).unwrap_or_else(|e| {
                lx_error!(
                    "session.grid_shm.from_fd_failed",
                    &format!("{e}"),
                    fd = fd
                );
                std::process::exit(1);
            });
            let (c, r) = (w.cols(), w.rows());
            lx_info!(
                "session.grid_shm.attached",
                "grid framebuffer from inherited fd",
                fd = fd,
                cols = c,
                rows = r
            );
            (w, c, r)
        }
        Err(_) => {
            let w = GridShmWriter::create(INITIAL_COLS, INITIAL_ROWS).unwrap_or_else(|e| {
                lx_error!("session.grid_shm.create_failed", &format!("{e}"));
                std::process::exit(1);
            });
            lx_info!(
                "session.grid_shm.self_created",
                "grid framebuffer self-created",
                fd = w.fd(),
                cols = INITIAL_COLS,
                rows = INITIAL_ROWS
            );
            (w, INITIAL_COLS, INITIAL_ROWS)
        }
    }
}

fn main() {
    marspot_term::logx::init("session");
    lx_event!(
        "SESSION_BOOT",
        "marspot-session starting",
        version = env!("MARSPOT_VERSION_SESSION"),
        marspot_version = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        pid = std::process::id()
    );

    // Event-driven wake: one channel carries both PTY byte/EOF wakes
    // (from either ShelldClient or LocalSession's reader thread) and
    // L2-forwarded keystrokes, so the main loop sleeps in a single
    // place until there's real work.
    let (ev_tx, ev_rx): (Sender<SessionEvent>, Receiver<SessionEvent>) = mpsc::channel();

    // RFC-003 step 1c: MARSPOT_L3_OWNS_PTY=1 picks the LocalSession
    // path. Default off; L4 shelld still owns PTY until L2 (Phase 3)
    // and the shelld delete (Phase 6) land.
    let owns_pty = std::env::var("MARSPOT_L3_OWNS_PTY").as_deref() == Ok("1");

    // Shared grid framebuffer first — it defines the geometry. When L2
    // owns the region it sized it to the on-screen cell rect; we must
    // drive the session at exactly those dims, since the grid we publish
    // has to fit the region (a mismatch would overflow the mapping).
    let (mut shm, cols, rows) = setup_shm();

    // Phase 2c: when we own the PTY, also own the UDS control socket
    // + registry entry so L2 (Phase 3) can discover us after a swap.
    // Bound for the whole owns-pty branch; Drop on exit unbinds +
    // prunes the on-disk entry.
    let mut _listener: Option<uds_server::SessionListener> = None;

    // Resumed control stream from the pre-execv image, if any —
    // populated by the resume branch below.  Lives outside `if
    // owns_pty` so `setup_control_socket` after the spawn can adopt
    // it.
    let mut resumed_poke: Option<UnixStream> = None;
    let mut resumed_client_generation: u64 = 0;
    let mut session: SessionImpl = if owns_pty {
        let wake_tx = ev_tx.clone();
        let wake = move || {
            let _ = wake_tx.send(SessionEvent::Wake);
        };
        let id = match std::env::var("MARSPOT_SESSION_ID")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => marspot_term::session_registry::allocate_next_session_id()
                .unwrap_or_else(|e| {
                    lx_error!("session.local.allocate_id_failed", &format!("{e}"));
                    std::process::exit(1);
                }),
        };
        // RFC-003 §6 Amendment 16 — two boot modes:
        //   * Resume: env `MARSPOT_L3_HANDOFF_MANIFEST=<path>` set →
        //             we're the post-execv image of a self-update.
        //             Read manifest, adopt master_fd + listen_fd from
        //             the pre-execv image's fd table (which survived
        //             via clear-CLOEXEC), apply Terminal snapshot,
        //             continue running.  Same PID, same shell child,
        //             zero perception by the user.
        //   * Cold:   plain fresh forkpty + shell + bind UDS listener.
        let local = if let Some(h) = try_resume_handoff() {
            // Adopt inherited PTY + listener — see do_l3_execv_swap.
            let mut terminal = marspot_term::terminal::Terminal::new(h.cols, h.rows);
            let state_path = session_state_bin_path(id);
            // F2+1 — mmap state.bin instead of std::fs::read so the
            // ~23 MB body never lands in libmalloc's MALLOC_LARGE
            // bucket.  apply_snapshot copies the bits it keeps into
            // Terminal/Grid Vecs; on Drop the mapping unmaps and the
            // pages go back to the kernel page cache (NOT to libmalloc
            // "empty").  See MappedFile docs for the full why.
            match MappedFile::open(&state_path) {
                Ok(mapped) => {
                    let body = mapped.as_slice();
                    let body_bytes = body.len();
                    if let Err(e) = terminal.apply_snapshot(body) {
                        lx_warn!(
                            "l3.execv.snapshot_apply_failed",
                            &format!("{e}"),
                            body_bytes = body_bytes
                        );
                    } else {
                        lx_event!(
                            "L3_EXECV_SNAPSHOT_APPLIED",
                            "restored Terminal from state.bin (mmap)",
                            body_bytes = body_bytes
                        );
                    }
                    drop(mapped);
                    let _ = std::fs::remove_file(&state_path);
                }
                Err(_) => {
                    lx_warn!(
                        "l3.execv.snapshot_missing",
                        "no state.bin; resumed Terminal blank until next PTY burst"
                    );
                }
            }
            lx_event!(
                "L3_EXECV_RESUMED",
                "adopting inherited PTY + UDS listener after execv",
                session_id = id,
                master_fd = h.master_fd,
                listen_fd = h.listen_fd,
                control_stream_fd = h.control_stream_fd,
                child_pid = h.child_pid
            );
            let local = LocalSession::from_handoff(
                id, h.master_fd, h.child_pid, h.cols, h.rows, terminal, wake,
            )
            .unwrap_or_else(|e| {
                lx_error!("l3.execv.from_handoff_failed", &format!("{e}"));
                std::process::exit(1);
            });
            // Adopt inherited UDS listener fd; no rebind, entry.toml
            // already has our (preserved) PID.
            match uds_server::SessionListener::from_handoff(
                id, h.listen_fd, h.child_pid, h.cols, h.rows, ev_tx.clone(),
            ) {
                Ok(l) => _listener = Some(l),
                Err(e) => {
                    lx_error!("l3.execv.listener_from_handoff_failed", &format!("{e}"));
                    std::process::exit(1);
                }
            }
            // Adopt the inherited control stream so L2's reader
            // doesn't see EOF across the execv.  Spawn the reader on
            // generation=1 (higher than the cold-start 0) so a stale
            // CoreGone from any race during the swap is recognised
            // and ignored.
            if h.control_stream_fd >= 0 {
                let stream = unsafe { UnixStream::from_raw_fd(h.control_stream_fd) };
                match stream.try_clone() {
                    Ok(writer) => {
                        resumed_client_generation = 1;
                        resumed_poke = Some(writer);
                        spawn_control_reader(stream, ev_tx.clone(), 1);
                        lx_event!(
                            "L3_EXECV_CONTROL_ADOPTED",
                            "adopted inherited L2 control stream across execv",
                            session_id = id,
                            stream_fd = h.control_stream_fd
                        );
                    }
                    Err(e) => {
                        lx_warn!(
                            "l3.execv.control_clone_failed",
                            &format!("{e}; will wait for L2 reconnect via UDS"),
                            stream_fd = h.control_stream_fd
                        );
                    }
                }
            }
            local
        } else {
            // Cold start: forkpty + shell + bind UDS.
            lx_event!(
                "L3_OWNS_PTY",
                "spawning local PTY (no shelld)",
                session_id = id,
                cols = cols,
                rows = rows
            );
            // F3+6 — if L2 supplied an initial cwd via env, propagate
            // it to LocalSession::spawn so the shell forks inside the
            // user's saved working directory (cold restart from
            // shell-state.bin path).  Empty / unset = $HOME fallback.
            let initial_cwd = std::env::var("MARSPOT_INITIAL_CWD").unwrap_or_default();
            let mut local = LocalSession::spawn(id, cols, rows, &initial_cwd, wake)
                .unwrap_or_else(|e| {
                    lx_error!("session.local.spawn_failed", &format!("{e}"));
                    std::process::exit(1);
                });
            // Cold-start resurrection:if a state.bin exists for this
            // session id,it's the dump a previous-life L3 wrote on
            // SIGTERM clean-exit(user closed window).Apply it to the
            // fresh Terminal so the user sees their saved scrollback
            // when they reopen the app.PTY child is brand-new, will
            // re-render its prompt over the saved screen — the
            // scrollback survives untouched (only the visible window
            // gets overwritten by the new shell's first paint).
            let state_path = session_state_bin_path(id);
            if state_path.exists() {
                match std::fs::read(&state_path) {
                    Ok(body) => {
                        let body_bytes = body.len();
                        match local.terminal_mut().apply_snapshot(&body) {
                            Ok(()) => {
                                lx_event!(
                                    "L3_COLD_SNAPSHOT_APPLIED",
                                    "restored Terminal from state.bin at cold boot — close→reopen resurrection",
                                    session_id = id,
                                    body_bytes = body_bytes
                                );
                            }
                            Err(e) => {
                                lx_warn!(
                                    "l3.cold.snapshot_apply_failed",
                                    &format!("{e}"),
                                    body_bytes = body_bytes
                                );
                            }
                        }
                    }
                    Err(e) => {
                        lx_warn!(
                            "l3.cold.snapshot_read_failed",
                            &format!("{e}"),
                            path = state_path.display()
                        );
                    }
                }
                // state.bin is one-shot — drop so a future SIGKILL doesn't
                // resurrect stale state on top of the new shell's real output.
                let _ = std::fs::remove_file(&state_path);
            }
            let cwd = std::env::var("HOME").unwrap_or_default();
            let shm_name = std::env::var("MARSPOT_SHM_NAME").unwrap_or_default();
            let shell_child_pid = local.child_pid();
            match uds_server::SessionListener::bind(
                id, cols, rows, &cwd, &shm_name, shell_child_pid, ev_tx.clone(),
            ) {
                Ok(l) => _listener = Some(l),
                Err(e) => {
                    lx_error!("session.local.uds_bind_failed", &format!("{e}"));
                    std::process::exit(1);
                }
            }
            local
        };
        SessionImpl::Local(local)
    } else {
        // RFC-003 Phase 6: L4 shelld retired.  MARSPOT_L3_OWNS_PTY=1
        // is now mandatory; the old shelld-driven fallback is gone.
        lx_error!(
            "session.shelld_path_removed",
            "MARSPOT_L3_OWNS_PTY=0 used to route through shelld; RFC-003 removed L4 — set MARSPOT_L3_OWNS_PTY=1 (or leave it unset and rely on the default)"
        );
        std::process::exit(1);
    };
    lx_event!(
        "SESSION_DRIVING",
        "driving session",
        session_id = session.id(),
        child_pid = session.child_pid(),
        cols = cols,
        rows = rows
    );

    // RFC-003 §6 Amendment 15 — install SIGTERM handler so L2 can
    // ask us to retire (silent-update path) or L1 can wipe us on
    // marspot quit.  The handler is just a self-pipe write; the main
    // loop turns the wake into a `ShutdownRequested` event and drives
    // the persist-state-then-exit path.  Best-effort: if install
    // fails the L3 still runs but silent updates degrade to "just
    // gets SIGKILL'd and loses state.bin".
    if let Err(e) = install_sigterm_handler(ev_tx.clone()) {
        lx_warn!(
            "l3.sigterm.install_failed",
            &format!("{e}; silent shutdown disabled for this L3")
        );
    } else {
        lx_event!(
            "L3_SIGTERM_INSTALLED",
            "SIGTERM handler armed for silent shutdown",
            pid = std::process::id()
        );
    }

    // Input source + L2 wake channel: L2 forwards keystrokes over the
    // control socket; we poke it back with GridReady after each publish.
    // Resume path: a control stream inherited across execv wins over
    // anything `setup_control_socket` would resolve from env (it's
    // already adopted + a reader is running on it).
    let mut poke = match resumed_poke.take() {
        Some(s) => Some(s),
        None => setup_control_socket(ev_tx.clone()),
    };
    // Scrollback view offset L2 last asked us to publish (0 = live tail).
    let mut view_offset: u16 = 0;
    // Track the grid's scroll-push counter so we can auto-pin
    // `view_offset` when the live grid scrolls a line into scrollback
    // while the user is viewing scrollback (`view_offset > 0`).  Without
    // this, every PTY line the shell emits while the user is scrolled
    // back shifts the visible content downward by one row — the
    // "老内容被新内容覆盖" symptom.  L2's `pump_all` runs the symmetric
    // bump so the two sides stay in lockstep without an extra
    // forward_scroll round-trip.
    let mut last_scroll_push: u64 = session.terminal().grid().scroll_push_count();
    publish_and_poke(&mut shm, &mut session, view_offset, poke.as_mut());

    // RFC-002 §4 (architectural correction over earlier step 6):
    // shelld (L4) owns the Terminal SoT now.  L3 no longer pushes
    // snapshots — the daemon parses every PTY byte itself, so on
    // every ATTACH it ships a freshly serialized state direct from
    // the master copy.  This eliminates the push throttle, the
    // generation-vector chase, and the "stale snapshot at the
    // server" failure mode entirely.

    // RFC-002 §8 (step 8b — fetch side): on attach, prefetch up to
    // HISTORY_PREFETCH_LINES rows of historic scrollback from L4 so
    // a user-initiated scroll-up has data to surface.  The reply is
    // drained from `take_pending_scrollback_pages` each publish tick
    // into `historic_pages`; integration into the publish window
    // (rendering above local scrollback) lands in step 8c.
    const HISTORY_PREFETCH_LINES: u32 = 1024;
    if let Err(e) = session.request_scrollback_page(0, HISTORY_PREFETCH_LINES) {
        lx_warn!(
            "session.scrollback.prefetch_send_failed",
            &format!("{e}"),
            session_id = session.id()
        );
    }
    let mut historic_pages: Vec<(Vec<marspot_term::grid::Cell>, bool)> = Vec::new();

    let start = Instant::now();
    let mut frame: u64 = 0;
    // RFC-003 §6 Amendment 11 (debug-3): generation tag for control
    // readers.  Increments on each NewClient adoption; CoreGone events
    // carry the generation of the reader that died, so a stale EOF
    // from a swapped-out L2 socket can't null the poke a newer L2
    // just adopted.
    let mut client_generation: u64 = resumed_client_generation;
    // RFC-003 §6 Amendment 16: SIGTERM watcher sets this; we break
    // the loop and the post-loop dispatcher decides between execv
    // handoff and clean-exit based on `should_execv_on_sigterm`.
    let mut want_shutdown = false;
    // B3 — in-flight scrollback search worker, or None.  At most one
    // worker per L3 at any time (last-write-wins, D15): a new
    // `SearchRequest` drops this Option, which sets the old worker's
    // cancel flag; the old worker exits at its next iteration without
    // delivering its batch.
    let mut current_search: Option<SearchWorker> = None;
    loop {
        let first = match ev_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                lx_event!("SESSION_EXIT", "event channel closed; exiting");
                break;
            }
        };
        // Drain the burst: handle every queued key now, coalesce wakes
        // into the single pump below, and collapse a flurry of resizes to
        // the final dims (intermediate sizes never need a reflow).
        let mut predicted = false;
        let mut pending_resize: Option<(u16, u16)> = None;
        let mut pending_scroll: Option<u16> = None;
        let mut selection_reqs: Vec<(u32, (u16, u32), (u16, u32), bool)> = Vec::new();
        let mut core_gone = false;
        for ev in first.into_iter().chain(std::iter::from_fn(|| ev_rx.try_recv().ok())) {
            match ev {
                SessionEvent::Key(e, m) => predicted |= handle_key(&mut session, e, m),
                SessionEvent::Resize(cols, rows) => pending_resize = Some((cols, rows)),
                SessionEvent::Scroll(off) => {
                    // L2(0.11.42+)mouse tracking on 时直接 encode +
                    // InjectInput,不再走 Scroll wire.所以这里只剩老
                    // scrollback path(non-mouse alt-screen 比如 less /
                    // vim 主屏 history).
                    lx_event!(
                        "L3_SCROLL_RECV",
                        "GridScroll received from L2",
                        session_id = session.id(),
                        off = off as u32
                    );
                    pending_scroll = Some(off);
                }
                // Each request gets its own reply (don't coalesce — L2 is
                // blocking on a reply per request).
                SessionEvent::GetSelection(seq, a, f, bw) => selection_reqs.push((seq, a, f, bw)),
                // Paste writes straight to the PTY; the echo round-trips
                // back through the normal pump → republish (no local
                // predict — bulk text isn't latency-sensitive like typing).
                SessionEvent::Paste(text) => handle_paste(&mut session, &text),
                SessionEvent::InjectInput(bytes) => {
                    // Raw write to PTY — no bracketed-paste, no key
                    // encoding.  Best-effort: a dead PTY just means
                    // the shell exited and we'll be torn down next.
                    let _ = session.write(&bytes);
                }
                SessionEvent::Wake => {}
                SessionEvent::CoreGone(gen) => {
                    // RFC-003 §6 Amendment 11 (debug-2 + debug-3 root
                    // cause): owns-pty L3 MUST NOT exit on control
                    // EOF, AND the EOF that ends an OLD client must
                    // not null out the poke a NEW client already
                    // adopted.
                    //
                    // Sequence during a dual-core swap:
                    //   T0: OLD L2 connected, control_reader@gen=K
                    //   T1: NEW L2 reattaches; NewClient fires →
                    //       client_generation=K+1, poke=NEW
                    //   T2: OLD L2 dies → OLD reader EOFs → fires
                    //       CoreGone(K)
                    //   T3: main sees CoreGone(K) — STALE; ignore.
                    //
                    // Without the gen tag, T3 would null poke and
                    // leave L3 disconnected from the live NEW L2.
                    if gen < client_generation {
                        lx_event!(
                            "L3_UDS_STALE_EOF",
                            "ignored stale control EOF; current client newer",
                            stale_gen = gen,
                            current_gen = client_generation
                        );
                    } else if owns_pty {
                        poke = None;
                        lx_event!(
                            "L3_UDS_CLIENT_GONE",
                            "control client closed; awaiting reattach (owns-pty)",
                            gen = gen
                        );
                    } else {
                        core_gone = true;
                    }
                }
                SessionEvent::ShutdownRequested => {
                    // RFC-003 §6 Amendment 16 — defer to the
                    // post-loop dispatcher so it can move `session`
                    // and `_listener` into `do_l3_execv_swap` (which
                    // needs ownership).  Drain the rest of this
                    // burst then break.
                    want_shutdown = true;
                }
                SessionEvent::SearchRequest {
                    query_id,
                    case_sensitive,
                    max_total,
                    query,
                } => {
                    // Last-write-wins (D15): cancel the prior worker
                    // (Drop sets its AtomicBool; it exits at its next
                    // iteration without delivering).  Any in-flight
                    // SearchHitsReady from the prior query is dropped
                    // by the qid-match check below.
                    if let Some(prev) = current_search.take() {
                        prev.cancel();
                    }
                    let snap = session
                        .terminal()
                        .grid()
                        .file_scrollback_snapshot();
                    match snap {
                        Some(snap) => {
                            // B4 — capture live grid alongside the
                            // file snapshot so unscrolled-yet rows
                            // are searched too.  Cell-clone cost is
                            // O(rows × cols) on the cold path (one
                            // per SearchRequest).  Hits remapped to
                            // u64::MAX-based synthetic indices in
                            // `spawn_search_merged`.
                            let live = LiveGridSnapshot::from_rows(
                                session
                                    .terminal()
                                    .grid()
                                    .live_grid_snapshot_for_search(),
                            );
                            let tx = ev_tx.clone();
                            let opts = SearchOpts {
                                case_sensitive,
                                max_total,
                            };
                            let worker = spawn_search_merged(
                                query_id,
                                snap,
                                live,
                                query,
                                opts,
                                move |qid, hits, has_more, total_seen| {
                                    let _ = tx.send(SessionEvent::SearchHitsReady {
                                        query_id: qid,
                                        hits,
                                        has_more,
                                        total_seen,
                                    });
                                },
                            );
                            lx_debug!(
                                "session.search.spawn",
                                "search worker spawned",
                                query_id = query_id,
                                max_total = max_total
                            );
                            current_search = Some(worker);
                        }
                        None => {
                            // No File-backed scrollback (Memory/Disk variant
                            // → opt-in env not set, or pre-FLIP default).
                            // Reply empty so the L2 search UI exits the
                            // "waiting" state.
                            if let Some(w) = poke.as_mut() {
                                let payload = encode_search_results(
                                    query_id,
                                    false,
                                    0,
                                    &[],
                                );
                                let _ = Frame::new(MsgType::SearchResults, payload)
                                    .write_to(w);
                            }
                            lx_debug!(
                                "session.search.no_file_scrollback",
                                "empty SearchResults sent (Memory/Disk variant)",
                                query_id = query_id
                            );
                        }
                    }
                }
                SessionEvent::SearchCancelRequested(query_id) => {
                    // Cancel only if it matches the current worker's qid.
                    // A late cancel for an older qid is a no-op (the
                    // old worker was already cancelled when the new
                    // SearchRequest arrived).
                    let matches = current_search
                        .as_ref()
                        .map(|w| w.query_id == query_id)
                        .unwrap_or(false);
                    if matches {
                        if let Some(w) = current_search.take() {
                            w.cancel();
                        }
                        lx_debug!(
                            "session.search.cancel",
                            "search worker cancelled by request",
                            query_id = query_id
                        );
                    }
                }
                SessionEvent::SearchHitsReady {
                    query_id,
                    hits,
                    has_more,
                    total_seen,
                } => {
                    // Last-write-wins: emit IFF this batch belongs to
                    // the current worker.  A batch from a worker that
                    // raced through to completion before we set its
                    // cancel flag (and got superseded by a newer
                    // SearchRequest before we processed it) lands
                    // here with a stale qid — drop it.
                    let live = current_search
                        .as_ref()
                        .map(|w| w.query_id == query_id)
                        .unwrap_or(false);
                    if live {
                        if let Some(w) = poke.as_mut() {
                            let payload =
                                encode_search_results(query_id, has_more, total_seen, &hits);
                            let _ = Frame::new(MsgType::SearchResults, payload).write_to(w);
                        }
                        // Worker has delivered; we can drop our
                        // handle.  Drop is a no-op cancel signal at
                        // this point (the worker has already exited).
                        current_search = None;
                        lx_debug!(
                            "session.search.results",
                            "SearchResults written to poke",
                            query_id = query_id,
                            total_seen = total_seen
                        );
                    } else {
                        lx_debug!(
                            "session.search.stale_batch",
                            "dropped stale SearchHitsReady",
                            query_id = query_id
                        );
                    }
                }
                SessionEvent::NewClient(stream) => {
                    // Adopt the validated stream as the control
                    // socket. spawn_control_reader fires inbound
                    // frames as SessionEvent::Key/Resize/etc just
                    // like the inherited-fd path.  Bump the
                    // generation BEFORE spawning so any in-flight
                    // CoreGone from the prior reader is recognised
                    // as stale by the time it reaches the loop.
                    client_generation += 1;
                    let this_gen = client_generation;
                    match stream.try_clone() {
                        Ok(writer) => {
                            poke = Some(writer);
                            spawn_control_reader(stream, ev_tx.clone(), this_gen);
                            // RFC-003 §6 Amendment 7: republish the
                            // current grid + fire GridReady so L2 has
                            // a poke to read the shm we already
                            // wrote.  Without this the first frame
                            // L2 sees is whatever the next PTY burst
                            // triggers — until then the pane renders
                            // empty (looks black).
                            publish_and_poke(
                                &mut shm,
                                &mut session,
                                view_offset,
                                poke.as_mut(),
                            );
                            lx_event!(
                                "L3_UDS_CLIENT_ADOPTED",
                                "control reader spawned on UDS stream"
                            );
                        }
                        Err(e) => {
                            lx_warn!(
                                "session.uds.try_clone_failed",
                                &format!("{e}")
                            );
                        }
                    }
                }
            }
        }
        // Our core vanished: the shelld session lives on (re-attached by
        // the next core), so exit rather than linger as an orphan.
        if core_gone {
            lx_event!("CORE_GONE", "L2/core gone (control EOF); exiting cleanly");
            break;
        }
        if want_shutdown {
            // Final pump before post-loop dispatcher decides
            // execv-handoff vs clean-exit.
            let _ = session.pump();
            publish_and_poke(&mut shm, &mut session, view_offset, poke.as_mut());
            break;
        }
        let resized = pending_resize.is_some();
        if let Some((cols, rows)) = pending_resize {
            // shelld ioctl's the PTY (SIGWINCH to the child) and the local
            // Terminal reflows; the next publish carries the new dims, and
            // L2's reader picks them up from the header with no remap.
            if let Err(e) = session.resize(cols, rows) {
                lx_warn!(
                    "session.resize_failed",
                    &format!("{e}"),
                    cols = cols,
                    rows = rows
                );
            }
        }
        // A scroll changes which window we publish even with no new output.
        let scrolled = match pending_scroll {
            Some(off) if off != view_offset => {
                view_offset = off;
                true
            }
            _ => false,
        };

        let n = session.pump();
        // Auto-pin view_offset on scrollback push: when the grid scrolls
        // a row into scrollback while the user is viewing history,
        // shift view_offset by the same delta so the visible window
        // tracks the same absolute content instead of sliding down.
        // L2 runs the symmetric bump in `pump_all` so the two stay in
        // sync without an extra forward_scroll round-trip.
        let cur_scroll_push = session.terminal().grid().scroll_push_count();
        if view_offset > 0 && cur_scroll_push > last_scroll_push {
            let delta = (cur_scroll_push - last_scroll_push).min(u16::MAX as u64) as u16;
            let max = session.terminal().grid().scrollback_len() as u16;
            // F1+8 — `.min(max)` would clamp view_offset DOWN when
            // the L3 just lost scrollback context (e.g. alt-screen
            // switch for a TUI like claudecode whose own scrollback
            // is shorter than the saved-main history we were
            // viewing).  After every PTY chunk that fired a
            // scroll_push, view_offset would snap to ~0 — looks
            // like "Enter 跳完马上弹回".  Skip the bump when max
            // < view_offset; keep the user's intentional position.
            if max >= view_offset {
                view_offset = view_offset.saturating_add(delta).min(max);
            } else {
                lx_event!(
                    "L3_BUMP_SKIP",
                    "skipping view_offset bump (max < view_offset)",
                    session_id = session.id(),
                    view_offset = view_offset as u32,
                    max_now = max as u32,
                    delta = delta as u32
                );
            }
        }
        last_scroll_push = cur_scroll_push;
        if session.is_exited() {
            session.pump();
            publish_and_poke(&mut shm, &mut session, view_offset, poke.as_mut());
            lx_event!("SESSION_EXITED", "shelld session exited; exiting cleanly");
            break;
        }
        // Republish on PTY output, a local echo that painted ahead of it,
        // a resize (grid shape changed), or a scroll (window changed) —
        // even when no bytes pumped this tick.
        if n > 0 || predicted || resized || scrolled {
            if scrolled {
                lx_event!(
                    "L3_PUBLISH",
                    "publish_and_poke after scroll",
                    session_id = session.id(),
                    view_offset = view_offset as u32,
                    cause = "scrolled"
                );
            }
            publish_and_poke(&mut shm, &mut session, view_offset, poke.as_mut());
        }

        // RFC-002 §8 (step 8b — fetch side): drain any ScrollbackPage
        // replies L4 sent.  Empty `line_count` means "no more history
        // past this point" — record the wall and stop asking.  Cells
        // accumulate in `historic_pages` (oldest-first per page, in
        // arrival order across pages) for the publish-side read-through
        // landing in step 8c.
        for page in session.take_pending_scrollback_pages() {
            match marspot_term::terminal::Terminal::decode_scrollback_page_body(
                page.line_count,
                &page.body,
            ) {
                Ok(mut lines) => {
                    let received = lines.len();
                    historic_pages.append(&mut lines);
                    lx_debug!(
                        "session.scrollback.page_received",
                        "RFC-002 GetScrollbackPage reply ingested",
                        line_start = page.line_start,
                        line_count = page.line_count,
                        received = received,
                        cached_total = historic_pages.len()
                    );
                }
                Err(e) => {
                    lx_warn!(
                        "session.scrollback.page_decode_failed",
                        &format!("{e}"),
                        line_start = page.line_start,
                        line_count = page.line_count,
                        body_bytes = page.body.len()
                    );
                }
            }
        }

        // Answer any Cmd-C selection requests against the post-pump grid.
        if !selection_reqs.is_empty() {
            if let Some(w) = poke.as_mut() {
                for (seq, anchor, focus, blockwise) in selection_reqs {
                    let text = grid_selection_text(session.terminal().grid(), anchor, focus, blockwise)
                        .unwrap_or_default();
                    let frame = Frame::new(MsgType::SelectionText, encode_selection_text(seq, &text));
                    if let Err(e) = frame.write_to(w) {
                        lx_warn!("session.selection_reply_failed", &format!("{e}"));
                        break;
                    }
                }
            }
        }

        frame += 1;
        // Heartbeat is for proof-of-life on idle sessions, not for
        // tracking byte activity — emitting on `n > 0` floods the
        // shared marspot.log at PTY-chunk rate (thousands per second
        // when a session is producing output), and the resulting
        // SINK mutex pressure across all live L3 processes was the
        // root cause of new-core HELLO timeouts during dual-core
        // swaps. Keep heartbeat purely time-based + lx_debug so it
        // only surfaces with MARSPOT_LOG_SESSION=debug.
        if frame.is_multiple_of(12) {
            let g = session.terminal().grid();
            let (cc, cr) = g.cursor();
            lx_debug!(
                "session.heartbeat",
                "pump tick",
                t_s = format!("{:.1}", start.elapsed().as_secs_f64()),
                pumped = n,
                cols = g.cols(),
                rows = g.rows(),
                cursor_col = cc,
                cursor_row = cr,
                state = state_str(session.state())
            );
        }
    }
    // RFC-003 §6 Amendment 16 — post-loop dispatcher.
    if want_shutdown {
        let id = session.id();
        let SessionImpl::Local(local) = session;
        let do_execv = should_execv_on_sigterm();
        if do_execv {
            if let Some(listener_owned) = _listener.take() {
                lx_event!(
                    "L3_SHUTDOWN_EXECV",
                    "fingerprint differs from current/marspot-session — execv",
                    session_id = id
                );
                match do_l3_execv_swap(id, local, listener_owned, poke.take()) {
                    Ok(()) => unreachable!(),
                    Err(e) => {
                        lx_error!(
                            "l3.execv.aborted",
                            &format!("{e}; exiting so L2 respawns this pane"),
                            session_id = id
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                lx_error!(
                    "l3.execv.no_listener",
                    "want_shutdown but no SessionListener; bug",
                    session_id = id
                );
                std::process::exit(1);
            }
        } else {
            // Clean exit: persist snapshot, leave Pty::Drop alone
            // (it SIGHUPs the shell — that's correct for user-quit).
            let body = local.terminal().serialize_snapshot();
            let path = session_state_bin_path(id);
            let _ = std::fs::write(&path, &body);
            lx_event!(
                "L3_SHUTDOWN_EXIT",
                "exiting on SIGTERM (fingerprint matches; clean exit)",
                session_id = id,
                pid = std::process::id()
            );
            std::process::exit(0);
        }
    }
}

// B3 integration tests — per `docs/scrollback-search.md` §7.2 +
// §12.B3.  These exercise the L3-side surface of the search rollout:
// `FileScrollback::snapshot_for_search()` + `spawn_search` worker +
// the last-write-wins dispatch contract.  They don't boot the full
// `main()` (which needs PTY + L2 socket) — instead they reproduce the
// state-machine moves the main loop makes and assert the observable
// behaviour the wire protocol promises.
#[cfg(test)]
mod b3_tests {
    use super::*;
    use marspot_term::grid::{Cell, CellAttrs};
    use marspot_term::scrollback::FileScrollback;
    use marspot_term::scrollback_search::{
        spawn_search, InMemorySource, SearchOpts, SearchSource, SearchWorker,
    };
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrd};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Throwaway temp dir for FileScrollback persistence.  Same shape
    /// as the `TmpDir` in `marspot-term::scrollback`'s tests; rebuilt
    /// here to keep the integration tests self-contained.
    struct TmpDir {
        path: std::path::PathBuf,
    }
    impl TmpDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, AtomicOrd::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir()
                .join(format!("marspot-b3-{label}-{pid}-{n}"));
            std::fs::create_dir_all(&dir).expect("tmpdir");
            Self { path: dir }
        }
        fn bin(&self) -> std::path::PathBuf {
            self.path.join("scrollback.bin")
        }
        fn idx(&self) -> std::path::PathBuf {
            self.path.join("scrollback.idx")
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn line_of(cols: usize, fill: char) -> Vec<Cell> {
        (0..cols)
            .map(|_| Cell {
                ch: fill,
                attrs: CellAttrs::default(),
            })
            .collect()
    }

    fn line_from_str(s: &str, cols: usize) -> Vec<Cell> {
        let mut out: Vec<Cell> = s
            .chars()
            .take(cols)
            .map(|c| Cell {
                ch: c,
                attrs: CellAttrs::default(),
            })
            .collect();
        while out.len() < cols {
            out.push(Cell {
                ch: ' ',
                attrs: CellAttrs::default(),
            });
        }
        out
    }

    /// 1 MB scrollback (≈ 10 k lines × 100 cols × ~24 B/cell + per-
    /// record overhead).  Search for "foo", first 64 hits must land
    /// in `< 50 ms` per the §4.6 budget for B3.
    #[test]
    fn b3_perf_1mb_first_64_hits_under_50ms() {
        let tmp = TmpDir::new("perf");
        let cols = 100usize;
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 1024)
            .expect("open FileScrollback");
        // Populate ~10 000 lines.  Every 100th line carries "foo" so
        // a max_total=64 scan will exit early after walking ~6 400
        // physical rows from the tail.
        let n = 10_000;
        for i in 0..n {
            let s = if i % 100 == 7 {
                format!("scratch line {i:05} foo bar baz qux quux corge grault garply waldo fred jim")
            } else {
                format!("scratch line {i:05} hello world abc def ghi jkl mno pqr stu vwx yz0 123 456")
            };
            sb.push_line(&line_from_str(&s, cols), false);
        }
        // Drop the live writer to flush its BufWriters.  The snapshot
        // for the worker reopens the file path with its own read fds.
        let snap = sb
            .snapshot_for_search()
            .expect("snapshot_for_search");
        drop(sb);

        let (tx, rx) = mpsc::channel::<(u32, Vec<WireSearchHit>, bool, u32, Duration)>();
        let opts = SearchOpts {
            case_sensitive: false,
            max_total: 64,
        };
        let started = Instant::now();
        let _worker = spawn_search(42, snap, "foo".to_string(), opts, move |qid, hits, hm, ts| {
            let _ = tx.send((qid, hits, hm, ts, started.elapsed()));
        });
        let (qid, hits, has_more, total_seen, elapsed) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker batch within 2 s");
        assert_eq!(qid, 42);
        assert_eq!(hits.len(), 64, "expected to fill max_total=64");
        assert!(has_more, "max_total cap reached should set has_more=true");
        assert!(total_seen >= 64);
        // §4.6 budget: 1 MB scrollback first 64 hits < 50 ms.  This is
        // the dev-box ceiling; bin/bench-remote enforces the mini
        // floor.  Generous on debug builds — release should be < 5 ms.
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(50)
        };
        assert!(
            elapsed < budget,
            "1 MB search first 64 hits should be < {budget:?}; got {elapsed:?}"
        );
    }

    /// Adapter that sleeps in `line()` so we can race the cancel flag
    /// against the search loop.  Reads from an `InMemorySource` so we
    /// don't pay file I/O on top of the synthetic delay.
    struct SlowSource {
        inner: InMemorySource,
        delay_us: u64,
    }
    impl SearchSource for SlowSource {
        fn line_count(&self) -> u64 {
            self.inner.line_count()
        }
        fn line(&self, idx: u64) -> Option<Vec<Cell>> {
            std::thread::sleep(Duration::from_micros(self.delay_us));
            self.inner.line(idx)
        }
        fn wrapped(&self, idx: u64) -> bool {
            self.inner.wrapped(idx)
        }
    }

    fn slow_source(rows: usize, delay_us: u64) -> SlowSource {
        let inner_rows: Vec<(Vec<Cell>, bool)> = (0..rows)
            .map(|i| {
                let s = format!("row {i:05} foo bar baz");
                let cells: Vec<Cell> = s
                    .chars()
                    .map(|c| Cell {
                        ch: c,
                        attrs: CellAttrs::default(),
                    })
                    .collect();
                (cells, false)
            })
            .collect();
        SlowSource {
            inner: InMemorySource { rows: inner_rows },
            delay_us,
        }
    }

    /// Cancelling an in-flight worker (by dropping its handle, the
    /// `SearchWorker::Drop` sets the cancel flag) must guarantee that
    /// `on_batch` never fires.  The §12.B3 DoD requires "worker exits
    /// within 1 ms of next iteration"; we give it a 250 ms wait
    /// window and assert nothing arrives.
    #[test]
    fn b3_cancel_drops_inflight_search() {
        let src = slow_source(1000, 500); // 500 µs × 1000 = 500 ms total
        let (tx, rx) = mpsc::channel::<u32>();
        // Hold an extra sender so the rx side stays open after the
        // worker's cloned sender drops (worker exits on cancel
        // without invoking the callback).
        let _keepalive = tx.clone();
        let opts = SearchOpts {
            case_sensitive: false,
            max_total: 1000,
        };
        let worker = spawn_search(7, src, "foo".to_string(), opts, move |qid, _hits, _hm, _ts| {
            let _ = tx.send(qid);
        });
        // Let the worker enter at least one row.
        std::thread::sleep(Duration::from_millis(5));
        // Drop = cancel.  Worker exits at its next cancel.load() check.
        drop(worker);
        // Wait long enough that the worker would have finished if not
        // cancelled (~500 ms uncancelled run) — assert no batch.
        match rx.recv_timeout(Duration::from_millis(250)) {
            Err(mpsc::RecvTimeoutError::Timeout) => { /* PASS — cancel suppressed batch */ }
            other => panic!("expected cancel to suppress batch; got {other:?}"),
        }
    }

    /// Last-write-wins: a new `SearchRequest` cancels the prior
    /// worker AND a stale `SearchHitsReady` from the cancelled worker
    /// (if it raced through before its cancel flag was checked) gets
    /// dropped by the main-loop dispatch.  We replay the dispatch
    /// state machine inline here without booting `main()`.
    #[test]
    fn b3_last_write_wins_new_query_supersedes_old() {
        let (tx, rx) = mpsc::channel::<(u32, Vec<WireSearchHit>, bool, u32)>();

        // Spawn A on a slow source — most likely cancelled mid-flight.
        let tx_a = tx.clone();
        let src_a = slow_source(2000, 500);
        let worker_a = spawn_search(
            100,
            src_a,
            "foo".to_string(),
            SearchOpts {
                case_sensitive: false,
                max_total: 5000,
            },
            move |qid, hits, hm, ts| {
                let _ = tx_a.send((qid, hits, hm, ts));
            },
        );
        let mut current: Option<SearchWorker> = Some(worker_a);

        // Brief delay so A is definitely scanning.
        std::thread::sleep(Duration::from_millis(5));

        // Replay main-loop SearchRequest handler: cancel prior worker
        // by dropping it, spawn new one.
        if let Some(prev) = current.take() {
            prev.cancel();
        }

        // B is a fast in-memory source — finishes promptly.
        let tx_b = tx.clone();
        let src_b = InMemorySource {
            rows: (0..50)
                .map(|i| {
                    let s = format!("row {i} foo");
                    let cells: Vec<Cell> = s
                        .chars()
                        .map(|c| Cell {
                            ch: c,
                            attrs: CellAttrs::default(),
                        })
                        .collect();
                    (cells, false)
                })
                .collect(),
        };
        let worker_b = spawn_search(
            101,
            src_b,
            "foo".to_string(),
            SearchOpts {
                case_sensitive: false,
                max_total: 64,
            },
            move |qid, hits, hm, ts| {
                let _ = tx_b.send((qid, hits, hm, ts));
            },
        );
        // Shadow `current` so the prior binding's last value is
        // dropped here cleanly without a dead-store warning.
        let current: Option<SearchWorker> = Some(worker_b);
        drop(tx); // No further producers; rx will see RecvTimeoutError after the workers finish.

        // Replay main-loop SearchHitsReady dispatch.  Loop until we've
        // collected the live batch (or timed out).  Any batch for a
        // qid that doesn't match `current.query_id` must be dropped.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut delivered: Vec<u32> = Vec::new();
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok((qid, _hits, _hm, _ts)) => {
                    let live = current
                        .as_ref()
                        .map(|w| w.query_id == qid)
                        .unwrap_or(false);
                    if live {
                        delivered.push(qid);
                        // Worker delivered.  We break out, so we don't
                        // bother clearing `current` here — Drop on
                        // scope exit takes care of it.
                        break;
                    }
                    // Stale → dropped, do not record.
                }
                Err(_) => break,
            }
        }

        assert_eq!(
            delivered,
            vec![101],
            "only B's qid should land in the dispatch sink; got {delivered:?}"
        );
        let _ = line_of; // silence dead-helper lint
    }
}
