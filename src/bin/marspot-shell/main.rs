//! marspot-shell — the outer process.
//!
//! Owns the NSWindow + NSApp event loop and a CAMetalLayer that
//! displays one IOSurface.  Spawns `marspot-core` as a child, hands it
//! the IOSurface ID via environment variables, and presents whatever
//! the child writes.
//!
//! Designed to be *boring* — anything substantive (parser, renderer,
//! input handling, layout) lives in the core process, behind a binary
//! we hot-swap during silent updates.  Keeping shell logic minimal
//! and dependencies thin means the shell almost never has to update,
//! which is what makes "no window flicker on upgrade" reachable.
//!
//! Step 1 scope: bring up the IOSurface link with a stub child that
//! draws a rotating gradient — proves the cross-process render path
//! before touching the real renderer.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Set by the SIGUSR1 handler.  The main-thread `poll_supervisor`
/// drains it and triggers `apply_pending_update`.  Atomic + flag
/// pattern is the only safe way to interact with the main thread
/// from a signal context (no NSApp / Metal / mutex calls allowed
/// inside `sigusr1_handler`).
static SIGUSR1_FLAG: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn sigusr1_handler(_signum: libc::c_int) {
    SIGUSR1_FLAG.store(true, Ordering::Release);
}

/// Launch journal for shell crash-loop detection: one
/// `ts \t current_mtime` TSV row per bundle-binary launch that is
/// about to redirect into `binaries/current/marspot-shell`.  Lives
/// next to the binaries tree so a `rm -rf Caches/marspot` resets
/// both together.
fn shell_launch_log_path() -> std::path::PathBuf {
    marspot::paths::shell_launch_journal()
}

/// ≥ this many launches of the same `current/` binary …
const LAUNCH_LOOP_THRESHOLD: usize = 3;
/// … within this window ⇒ crash loop (the redirected shell is dying
/// before the user can even interact with it).
const LAUNCH_LOOP_WINDOW_SECS: f64 = 60.0;
/// Journal stays bounded: once it crosses this many lines we rewrite
/// it down to the trailing half.  One row per launch, so this is
/// generous.
const LAUNCH_LOG_MAX_LINES: usize = 64;

/// Append this launch to the journal, then report whether the last
/// `LAUNCH_LOOP_THRESHOLD` rows (including this one) all point at the
/// same `current/` binary (by mtime) inside `LAUNCH_LOOP_WINDOW_SECS`.
/// That signature means the binary we keep redirecting into never
/// lives long enough to matter — a broken self-update would otherwise
/// wedge the app in an exec → crash → relaunch loop forever.
fn record_launch_and_detect_loop(current: &std::path::Path) -> bool {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    let mtime = match std::fs::metadata(current).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        Err(_) => return false,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let path = shell_launch_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{now}\t{mtime}");
    }
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() > LAUNCH_LOG_MAX_LINES {
        let tail = lines[lines.len() - LAUNCH_LOG_MAX_LINES / 2..].join("\n");
        let _ = std::fs::write(&path, tail + "\n");
    }
    // Newest-first; rows that fail to parse (hand-edited file) just
    // don't count toward the threshold.
    let recent: Vec<(f64, u64)> = lines
        .iter()
        .rev()
        .take(LAUNCH_LOOP_THRESHOLD)
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    if recent.len() < LAUNCH_LOOP_THRESHOLD {
        return false;
    }
    let span = recent[0].0 - recent[recent.len() - 1].0;
    recent.iter().all(|r| r.1 == mtime) && span >= 0.0 && span <= LAUNCH_LOOP_WINDOW_SECS
}

/// Re-exec into `binaries/current/marspot-shell` if it exists and
/// is a different file from us.  Guarded against infinite recursion
/// by `MARSPOT_NO_REDIRECT=1` (set on the env we pass to the new
/// process, and also a user escape hatch for "run THIS bundle
/// binary even if a current exists").
///
/// Only returns on:
///   - guard env set,
///   - no current/marspot-shell,
///   - current is the same file we already are,
///   - crash-loop rollback left current/ empty (run as ourselves), or
///   - exec failed (logged, then we proceed as ourselves).
fn maybe_redirect_to_current_shell() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("MARSPOT_NO_REDIRECT").is_some() {
        return;
    }
    let tree = match supervisor::BinaryTree::for_shell() {
        Ok(t) => t,
        Err(_) => return,
    };
    let current = tree.current();
    if !current.exists() {
        return;
    }
    let me = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    // If we ARE current, no redirect needed.  canonicalize handles
    // symlinks but is fine if either path doesn't symlink.
    let me_c = me.canonicalize().unwrap_or_else(|_| me.clone());
    let cur_c = current.canonicalize().unwrap_or_else(|_| current.clone());
    if me_c == cur_c {
        return;
    }
    // Crash-loop guard: if this same current/ binary keeps getting
    // launched and (evidently) dying, stop redirecting into it.
    // Quarantine it and restore prev/ — or, with no prev/, leave
    // current/ empty so this launch (and future ones) run the
    // bundle binary that's known to at least start.
    //
    // CLI invocations (`--status`, `--version`, …) also redirect but
    // exit quickly by design — they are not crash evidence, so they
    // stay out of the journal (three `--status` calls in a minute
    // must not roll back a healthy shell).
    let is_gui_launch = std::env::args_os().nth(1).is_none();
    if is_gui_launch && record_launch_and_detect_loop(&current) {
        match tree.rollback_to_prev() {
            Ok(true) => sup_log::log(
                "SHELL_AUTO_ROLLBACK",
                "crash loop on current/ — quarantined, restored prev/",
            ),
            Ok(false) => sup_log::log(
                "SHELL_AUTO_ROLLBACK",
                "crash loop on current/ — quarantined, no prev/, running bundle binary",
            ),
            Err(e) => sup_log::log(
                "SHELL_AUTO_ROLLBACK",
                &format!("crash loop on current/ — rollback failed: {e}"),
            ),
        }
        if !current.exists() {
            return;
        }
    }
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let arg0 = args.first().cloned().unwrap_or_else(|| current.clone().into());
    // Stash our bundle dir (parent of the current_exe) so the new
    // shell can fall back there when resolving marspot-core.
    let bundle_dir = me
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let err = std::process::Command::new(&current)
        .arg0(arg0)
        .args(args.iter().skip(1))
        .env("MARSPOT_NO_REDIRECT", "1")
        .env("MARSPOT_BUNDLE_DIR", &bundle_dir)
        .exec();
    // exec only returns on failure.
    lx_error!(
        "shell.redirect_failed",
        &format!("{err} — running bundle binary instead"),
        target = current.display()
    );
}

/// `--rollback-shell` / `--rollback-core`: quarantine `current/` and
/// restore `prev/` for the named layer.  Dispatched BEFORE the
/// current/ redirect — the whole point of the command is that
/// current/ may be broken, so it must run in the bundle binary the
/// user actually invoked, not be exec'd into the broken one.
/// Doesn't touch a running shell; restart Marspot to pick up the
/// restored binary.
fn cmd_rollback(which: &str) -> i32 {
    let tree = match which {
        "shell" => supervisor::BinaryTree::for_shell(),
        _ => supervisor::BinaryTree::for_core(),
    };
    let tree = match tree {
        Ok(t) => t,
        Err(e) => {
            eprintln!("marspot-shell: rollback {which}: {e}");
            return 1;
        }
    };
    match tree.rollback_to_prev() {
        Ok(true) => {
            sup_log::log(
                "MANUAL_ROLLBACK",
                &format!("{which}: quarantined current/, restored prev/"),
            );
            println!("{which}: rolled back — current/ quarantined, prev/ restored.");
            println!("Restart Marspot to run the restored binary.");
            0
        }
        Ok(false) => {
            sup_log::log(
                "MANUAL_ROLLBACK",
                &format!("{which}: quarantined current/, no prev/ — bundle fallback"),
            );
            println!(
                "{which}: current/ quarantined; no prev/ to restore — \
                 next launch falls back to the bundle binary."
            );
            0
        }
        Err(e) => {
            sup_log::log("MANUAL_ROLLBACK", &format!("{which}: failed: {e}"));
            eprintln!("marspot-shell: rollback {which} failed: {e}");
            1
        }
    }
}

fn print_version() {
    // marspot-shell shows its own L1 version. Operators reading
    // --version want to know "what supervisor / NSWindow owner am I
    // running?" — distinct from "what marspot am I on?" (= L2 / core).
    println!(
        "marspot-shell {} (git {} built {})  — marspot {} (core)",
        env!("MARSPOT_VERSION_SHELL"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        env!("MARSPOT_VERSION_CORE")
    );
}

/// Find the live shell PID via `/proc`-less ps lookup.  Returns the
/// first matching PID (there should normally only be one); returns
/// `None` if no shell is running.  Excludes our own PID so the
/// `--status` invocation never sees itself.
fn find_running_shell_pid() -> Option<u32> {
    let mine = std::process::id();
    let out = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid,comm"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines().skip(1) {
        let mut it = line.split_whitespace();
        let pid: u32 = it.next()?.parse().ok()?;
        if pid == mine {
            continue;
        }
        let rest: String = it.collect::<Vec<_>>().join(" ");
        // `comm` may carry the full path; match the basename.
        if rest
            .rsplit('/')
            .next()
            .map(|n| n == "marspot-shell")
            .unwrap_or(false)
        {
            return Some(pid);
        }
    }
    None
}

fn print_status() {
    print_version();
    println!();

    // Live processes (pid file for this state dir, else ps scan).
    match running_shell_pid() {
        Some(pid) => println!("Running supervisor: pid {pid}"),
        None => println!("Running supervisor: (none)"),
    }
    // Anchor `marspot-core($| )` rather than the bare name: a binary
    // name can be a prefix of a sibling's (the way `marspot-shell`
    // matches `marspot-shelld`), so an unanchored `-f` match risks
    // catching the wrong process. Match at end-of-argv or before args.
    let core_pids: Vec<String> = match std::process::Command::new("/usr/bin/pgrep")
        .args(["-f", "marspot-core($| )"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    };
    println!("Running core(s): {}", core_pids.join(", "));
    println!();

    // Latest events from supervisor.log.
    let log = sup_log_path();
    println!("Recent supervisor events (tail of {}):", log.display());
    match std::fs::read_to_string(&log) {
        Ok(s) => {
            let lines: Vec<&str> = s.lines().collect();
            let n = lines.len();
            let take = 12.min(n);
            for line in &lines[n - take..] {
                let mut it = line.splitn(3, '\t');
                let ts: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let tag = it.next().unwrap_or("?");
                let detail = it.next().unwrap_or("");
                // Format the unix-seconds-f64 as a local datetime via `date`.
                let ts_s = std::process::Command::new("/bin/date")
                    .args(["-r", &format!("{:.0}", ts), "+%Y-%m-%d %H:%M:%S"])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                println!("  {ts_s}  {:<18}  {detail}", tag);
            }
        }
        Err(e) => println!("  (could not read: {e})"),
    }
}

fn sup_log_path() -> std::path::PathBuf {
    marspot::paths::supervisor_log()
}

/// `--trigger` — send SIGUSR1 to the running shell so it applies any
/// staged pending update.  Returns the process exit code:
///   0 = signal sent successfully
///   1 = no running shell found
///   2 = signal call errored
/// Record this GUI shell's pid under the state dir.  Best-effort —
/// `--trigger` falls back to a `ps` scan if the file is missing.
fn write_shell_pid() {
    let path = marspot::paths::shell_pid_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, std::process::id().to_string());
}

/// Pid of the running GUI shell for THIS state dir.  Prefers the pid
/// file (state-dir-scoped, so a sandbox shell and the installed
/// shell never cross-fire); falls back to a `ps` basename scan when
/// the file is absent or its pid is dead.
fn running_shell_pid() -> Option<u32> {
    let path = marspot::paths::shell_pid_file();
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(pid) = s.trim().parse::<u32>() {
            // kill(pid, 0): 0 = alive and ours to signal.
            if pid != std::process::id()
                && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
            {
                return Some(pid);
            }
        }
    }
    find_running_shell_pid()
}

fn cmd_trigger() -> i32 {
    match running_shell_pid() {
        Some(pid) => {
            // SAFETY: libc::kill is a syscall wrapper; no Rust invariants.
            let r = unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
            if r == 0 {
                println!("sent SIGUSR1 to marspot-shell pid {pid}");
                0
            } else {
                eprintln!(
                    "kill failed: {}",
                    std::io::Error::last_os_error()
                );
                2
            }
        }
        None => {
            eprintln!("no running marspot-shell to trigger");
            1
        }
    }
}

fn install_sigusr1_handler() {
    // SAFETY: registering a handler is signal-safe; the handler we
    // register only touches an AtomicBool.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            sigusr1_handler as *const () as libc::sighandler_t,
        );
    }
}

use marspot::{lx_debug, lx_debug_sampled, lx_error, lx_event, lx_info, lx_warn};
use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::shell_proto::{
    decode_caret_rect, decode_hello_ack, decode_pong, decode_surface_ready, encode_focus,
    encode_hello, encode_key_event, encode_mouse, encode_ping, encode_preedit,
    encode_scroll, encode_surface_attach, event_to_wire, struct_to_mods_byte, Frame, MsgType,
    DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID,
    ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};

mod banner;
mod plugins;
mod present;
mod sup_log;
mod supervisor;
use banner::BannerKind;
use plugins::host::ShellPluginHost;
use plugins::PluginRegistry;
use present::ShellPresenter;
use supervisor::{BinaryTree, SupervisorState};

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
// Safety-net only: the shell presents on the core's per-frame
// `FrameRendered` poke, so this timer just guarantees forward progress if a
// poke is ever missed (it never freezes).  Was 16 ms (~60 fps blind
// present) — that burned idle CPU and occasionally sampled the IOSurface
// mid-render (a flicker).  250 ms = 4 Hz idle floor, negligible CPU.
const REDRAW_INTERVAL_MS: u64 = 250;

/// Frames the reader thread parses off the control socket and hands
/// to the main thread.
enum ShellInbox {
    SurfaceReady(u32),
    HelloAck(u32),
    Pong(u32),
    /// Focused-pane caret rect from the core (view-local physical
    /// pixels), forwarded to AppKit so the IME candidate window
    /// anchors under the caret.
    CaretRect(Option<(f64, f64, f64, f64)>),
    /// Core rendered a fresh frame into the IOSurface — present it.  Carries
    /// nothing; its arrival (via the reader's `proxy.wake()`) drives the
    /// per-frame present, replacing the blind ~60 fps redraw timer.
    FrameRendered,
}

/// How long after spawn we expect HELLO_ACK before declaring the core
/// hung at startup. Generous enough to cover a cold-cache first-run
/// of a freshly-built binary: macOS Gatekeeper provenance check on
/// the just-promoted current/marspot-core (~400 ms when warm, more
/// on cold cache) PLUS 9 cold L3 spawns (each another fresh-exec
/// provenance check) PLUS ShelldClient handshake PLUS list_sessions/
/// attach RPC roundtrips. 5 s used to be enough, but install-local's
/// silent update on a fresh release build occasionally tripped past
/// it (observed on 0.2.10 → 0.2.11), so bump to 15 s. A *real* hang
/// is still caught — the PONG_DEADLINE (15 s × 3 PING_INTERVALs) is
/// the steady-state liveness check.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// How often we issue PING.
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// PONG must arrive within this many `PING_INTERVAL`s before we call
/// the core hung.  3 = 15 s, which gives plenty of slack for a busy
/// terminal session without making a real hang feel sticky.
const PONG_DEADLINE: Duration = Duration::from_secs(15);
/// Crash-budget window.  More than `MAX_CRASHES_IN_WINDOW` in this
/// span and we stop auto-restarting (binary is broken; user needs
/// to roll back or reinstall).
const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 min
const MAX_CRASHES_IN_WINDOW: usize = 3;

/// Everything tied to one live core process: its child handle, the
/// control socket (both directions), and the liveness-handshake
/// bookkeeping.  Aggregating these is what makes a flash-free silent
/// update possible — the shell can hold an `active` and a `pending`
/// CoreConn at once, let a fresh core prove itself, and only then
/// atomic-swap the presenter and tear the old one down.
///
/// `child` is `Option` because the child can legitimately be gone
/// while the rest of the conn lives: after a HELLO mismatch we kill
/// the child but keep `spawned_at` so the HELLO-timeout path in
/// `poll_supervisor` respawns; and `poll_supervisor` reaps the child
/// (`child.take()`) before `restart_core` drops the whole conn.
struct CoreConn {
    /// The core child process, or `None` once killed/reaped.
    child: Option<Child>,
    /// Parent end of the AF_UNIX socketpair we share with the core.
    /// Wrapped in `Mutex` so the `MarspotApp` callbacks (all on the
    /// main thread, but the type system doesn't know that) can mutate
    /// it without splitting the struct.  Frames written here arrive
    /// at the core's stdin-side fd 3 / `MARSPOT_SHELL_CONTROL_FD`.
    control_tx: Mutex<UnixStream>,
    /// Receives parsed inbound frames from this core's reader thread.
    control_rx: Receiver<ShellInbox>,
    /// True after this core has confirmed at least one HelloAck.
    hello_acked: bool,
    /// When this core was spawned — drives the HELLO timeout.
    spawned_at: Instant,
    /// When to fire the next PING to this core.
    next_ping_at: Instant,
    /// Nonce of the most recent PING we sent this core.  Pongs with a
    /// different nonce are stale and ignored.  Per-conn: a fresh core
    /// gets its own channel, so a previous core's Pong can't reach it.
    last_ping_nonce: u32,
    /// When the most recent matching Pong arrived.
    last_pong_at: Instant,
}

impl CoreConn {
    /// Write a frame to this core.  Logs and drops on EPIPE; a core
    /// dying mid-session is the supervisor's problem, not this path's.
    fn send(&self, msg_type: MsgType, payload: Vec<u8>) {
        let frame = Frame::new(msg_type, payload);
        let mut stream = match self.control_tx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: still try
        };
        if let Err(e) = frame.write_to(&mut *stream) {
            lx_warn!(
                "shell.control_frame_write_failed",
                &format!("{e}"),
                msg_type = format!("{:?}", msg_type)
            );
        }
    }

    /// Drop the control socket (clean EOF for the core's read side),
    /// then SIGKILL + reap the child.  Order matters: the EOF gives a
    /// well-behaved core the chance to exit on its own before the kill.
    fn shutdown(self) {
        let CoreConn { child, control_tx, control_rx, .. } = self;
        drop(control_tx);
        drop(control_rx);
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// PROTO_VERSION=2 double-buffer IOSurface pair.  The core alternates
/// writing into `front` and `back`; after each render+wait it acks
/// `SurfaceReady(id)`.  The shell never samples a surface the core is
/// mid-writing — that race was the dominant flash source diagnosed
/// 2026-06-15.  Owns IOSurface use-counts: dropping the pair via
/// `release()` decrements both.
struct SurfacePair {
    front: IOSurface,
    back: IOSurface,
}

impl SurfacePair {
    /// Create both surfaces at the same dimensions and `increment_use`
    /// on each so they survive past initial return.  `release()`
    /// (drop-style helper) decrements them when the pair is retired.
    fn create(w_px: usize, h_px: usize) -> Result<Self, String> {
        let front = IOSurface::create(w_px, h_px)?;
        front.increment_use();
        let back = match IOSurface::create(w_px, h_px) {
            Ok(s) => {
                s.increment_use();
                s
            }
            Err(e) => {
                front.decrement_use();
                return Err(e);
            }
        };
        // IOSurface lifecycle is the load-bearing fact in every flash
        // / black-window incident — surface it explicitly at DEBUG.
        // pair create / set_pair / release / swap together let a future
        // incident reconstruct exactly which surfaces a given frame was
        // sampling.
        lx_debug!(
            "shell.surface.create_pair",
            "IOSurface pair created",
            front_id = front.id(),
            back_id = back.id(),
            w_px = w_px,
            h_px = h_px
        );
        Ok(Self { front, back })
    }

    fn ids(&self) -> (u32, u32) {
        (self.front.id(), self.back.id())
    }

    fn width(&self) -> usize {
        self.front.width()
    }

    fn height(&self) -> usize {
        self.front.height()
    }

    /// Drop reverse: decrement_use on both halves.  Pair is consumed
    /// because both surfaces become invalid for the shell after this.
    fn release(self) {
        lx_debug!(
            "shell.surface.release_pair",
            "IOSurface pair released",
            front_id = self.front.id(),
            back_id = self.back.id()
        );
        self.front.decrement_use();
        self.back.decrement_use();
    }
}

/// A silent update in flight.  The shell promotes the new binary,
/// spawns a `pending` core into a *fresh* IOSurface pair, and lets it
/// rebuild the screen off-screen (from shelld's bytelog) while the
/// `active` core keeps rendering the surface the user is looking at.
/// Once the pending core has HelloAck'd, reported its surface ready,
/// and survived probation, the shell atomic-swaps the presenter to the
/// new pair and retires the old core — no flash, because both pairs
/// carry identical content.  On any failure the pending core is killed
/// and the binary rolled back; the user never sees a glitch.
struct PendingUpdate {
    /// The probationary core, rendering into `surfaces`.
    conn: CoreConn,
    /// The fresh IOSurface pair the pending core draws into.  Becomes
    /// the displayed pair on a successful swap; released on abort.
    surfaces: SurfacePair,
    /// True once the pending core confirmed `SurfaceReady` for one of
    /// the pair's ids — proof it can actually render.
    surface_ready: bool,
}

struct ShellApp {
    proxy: EventProxy,
    /// Currently-displayed IOSurface pair — the presenter samples
    /// whichever half `current_idx` points at.
    surfaces: Option<SurfacePair>,
    /// Created in `resized` (or `restart_core`) and not yet promoted.
    /// Once the core confirms via `SurfaceReady(id)` matching either
    /// id of this pair, we install it as the live pair and let the
    /// presenter swap to the ready id.  A later resize replaces the
    /// pending entry; the dropped one is abandoned (`release`).
    pending_surfaces: Option<SurfacePair>,
    presenter: Option<ShellPresenter>,
    /// The live core: child process, control socket (both directions),
    /// and liveness-handshake state, aggregated into `CoreConn` so a
    /// second one (`pending`) can be held during a flash-free
    /// silent-update swap.  `None` before the first spawn and in the
    /// brief gap between a crash and the respawn.
    active: Option<CoreConn>,
    /// A silent update on probation: a second core rendering the same
    /// screen into its own surface, waiting to be swapped in.  `None`
    /// outside an update.  Present iff `sup_state` is `Probation`.
    pending: Option<PendingUpdate>,
    /// True after the core has confirmed at least one SurfaceReady.
    /// Until then `redraw` skips `present()` so the user sees the
    /// NSWindow's BG colour (font_cache::BG) instead of an unfilled
    /// black IOSurface — kills the cold-start flash.
    first_frame_ready: bool,
    redraw_thread_started: bool,
    /// `redraw()` only calls `present()` when this is true.  Set by
    /// every `FrameRendered` poke from the active core (its
    /// per-frame ack that the IOSurface has been fully written +
    /// waitUntilCompleted'd).  Cleared by `redraw` after `present()`.
    ///
    /// Why: the 250 ms safety-net timer in `start_redraw_pump`
    /// wakes the AppKit redraw callback unconditionally — if the
    /// callback then samples the IOSurface, it can land MID-RENDER
    /// (between core's BG pass and FG glyph pass), producing the
    /// frequent "all 9 panes' contents momentarily disappear and
    /// reappear" flash documented in the 2026-06-15 debugging.
    /// Gating presents on FrameRendered means the safety-net timer
    /// only forces a present when a frame is actually new *and*
    /// AppKit hasn't already picked it up via the poke fast-path.
    /// `safety_present_after` below provides the real safety net
    /// for genuinely-missed pokes.
    frame_pending: bool,
    /// Wall-clock time of the last `present()` call.  Combined with
    /// `frame_pending`: when the redraw callback runs and no fresh
    /// frame is pending, we ALSO force a present if too long has
    /// elapsed since the last one — covers the pathological
    /// "core froze mid-render and no future poke is coming" case
    /// where waiting for FrameRendered would freeze the window.
    last_present_at: Option<Instant>,
    /// Binary slot manager: current / prev / pending.  Used to find
    /// the core binary at spawn time and to atomic-swap when a
    /// silent update fires.
    binaries: BinaryTree,
    /// Where in the silent-update lifecycle we are.  `Idle` most of
    /// the time; flips to `Probation` after we promote a new core.
    sup_state: SupervisorState,
    /// Recent crash timestamps inside the `CRASH_WINDOW` rolling
    /// window.  Used to refuse auto-restart on a binary that's
    /// flapping.
    crashes: std::collections::VecDeque<Instant>,
    /// True if the crash budget has been blown.  We stop trying to
    /// restart until something external changes (manual update,
    /// shell relaunch).
    auto_restart_disabled: bool,
    /// Currently-displayed banner, or `None` for clear.  Kept on
    /// the shell so `poll_supervisor` can recompute it from state
    /// transitions and call `presenter.set_banner` only when it
    /// actually changes.
    banner_kind: Option<BannerKind>,
    /// Rolling 30 s ring of CORE_SPAWN timestamps.  When this fills
    /// (≥3 entries inside the window) we emit a `CORE_BOOT_LOOP`
    /// WARN — the alarm the 2026-06-15 incident lacked.  In that
    /// case six cores booted in three minutes and the operator had
    /// to manually count `grep CORE_BOOT` lines to spot the loop.
    /// Bound at 16 so even a runaway respawn can't grow the ring.
    core_boot_ring: std::collections::VecDeque<Instant>,
    /// RFC-001 plugin host.  Plugins read pane / pty info through it.
    /// Registry owns the loaded plugins + drives their lifecycle.
    /// Both `None`-able so a misbuilt host (env-var disabled etc.)
    /// gracefully degrades to plugin-less marspot.
    plugin_host: ShellPluginHost,
    plugin_registry: PluginRegistry,
    /// When the supervisor last drove plugin ticks.  Coarse — the
    /// supervisor itself runs at ~250 ms, plugins requesting < 250 ms
    /// just see the supervisor cadence.  Their per-plugin interval
    /// gating lives in the registry.
    last_plugin_tick: Instant,
    /// Receiver half of the channel `ShellPluginHost::set_pane_badge`
    /// pushes into; drained each `poll_supervisor` tick and forwarded
    /// to the active core as `MsgType::PaneBadge` frames.
    pane_badge_rx: std::sync::mpsc::Receiver<plugins::host::PaneBadgeUpdate>,
}

impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        let (pane_badge_tx, pane_badge_rx) = std::sync::mpsc::channel();
        Self {
            proxy,
            surfaces: None,
            pending_surfaces: None,
            presenter: None,
            active: None,
            pending: None,
            first_frame_ready: false,
            redraw_thread_started: false,
            frame_pending: false,
            last_present_at: None,
            binaries,
            sup_state: SupervisorState::Idle,
            crashes: std::collections::VecDeque::new(),
            auto_restart_disabled: false,
            banner_kind: None,
            core_boot_ring: std::collections::VecDeque::with_capacity(16),
            plugin_host: {
                let h = ShellPluginHost::new();
                h.attach_pane_badge_tx(pane_badge_tx);
                h
            },
            plugin_registry: PluginRegistry::new(),
            last_plugin_tick: Instant::now() - Duration::from_secs(1),
            pane_badge_rx,
        }
    }

    /// Tear down the active core (clean EOF then SIGKILL + reap) and
    /// clear the slot.  No-op when there's no active core.
    ///
    /// `reason` is the forensic anchor — every caller passes a short
    /// static string saying WHY we're closing the connection (crash
    /// detected, manual update, window close, etc.).  Without this
    /// the supervisor sees `CORE_EXIT control socket closed by shell`
    /// in `marspot.log` and the only thing the user can say is "okay,
    /// shell closed it — but why?".  The 2026-06-15 incident debugging
    /// loop made this hole obvious: six core boots in three minutes
    /// and no log entry telling us which path was firing them.
    fn shutdown_active(&mut self, reason: &'static str) {
        if let Some(conn) = self.active.take() {
            let pid = conn.child.as_ref().and_then(|c| Some(c.id())).unwrap_or(0);
            lx_event!(
                "ACTIVE_SHUTDOWN",
                "tearing down active core",
                reason = reason,
                pid = pid
            );
            conn.shutdown();
        } else {
            // No-op path is still worth logging — it tells us a
            // shutdown was requested when no core was live (race
            // between supervisor signals).
            lx_debug!(
                "shell.shutdown_active.noop",
                "shutdown_active called but slot was empty",
                reason = reason
            );
        }
    }

    /// Record a CORE_SPAWN into the rolling ring and flag a
    /// boot-loop alarm if the ring's 30 s window now holds ≥3 boots.
    /// Cheap O(N) on a VecDeque bounded at 16 — the loop check is
    /// already paying VecDeque ops to maintain the ring; the
    /// occasional WARN is dwarfed by everything else in `poll_supervisor`.
    fn record_core_boot(&mut self) {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(30);
        let cutoff = now - window;
        while self
            .core_boot_ring
            .front()
            .map(|t| *t < cutoff)
            .unwrap_or(false)
        {
            self.core_boot_ring.pop_front();
        }
        self.core_boot_ring.push_back(now);
        // Cap the ring even if eviction lags (defensive).
        while self.core_boot_ring.len() > 16 {
            self.core_boot_ring.pop_front();
        }
        let count = self.core_boot_ring.len();
        if count >= 3 {
            lx_event!(
                "CORE_BOOT_LOOP",
                "core respawn loop detected — see CORE_SPAWN/CORE_EXIT timeline",
                count = count,
                window_s = 30
            );
            sup_log::log(
                "CORE_BOOT_LOOP",
                &format!("count={count} window_s=30"),
            );
        }
    }

    /// True when the active core has a live child process.  False
    /// before the first spawn, in the crash→respawn gap, and after a
    /// HELLO mismatch kill (where the conn lingers but `child` is gone).
    fn core_alive(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|c| c.child.as_ref())
            .is_some()
    }

    /// Send a frame to the active core.  Input, resize, ping, and
    /// focus all target `active` only — a `pending` core on probation
    /// receives none of them (it rebuilds independently from shelld).
    fn send(&self, msg_type: MsgType, payload: Vec<u8>) {
        if let Some(conn) = self.active.as_ref() {
            conn.send(msg_type, payload);
        }
    }

    /// Spawn a core process rendering into `surface_id` and return the
    /// `CoreConn` (HELLO already sent on its own socket).  Returns
    /// `None` on any spawn failure.  The caller decides whether the new
    /// core becomes `active` (boot / restart) or `pending` (an update on
    /// probation) — this fn touches neither slot.
    fn spawn_core(
        &self,
        front_id: u32,
        back_id: u32,
        w_phys: usize,
        h_phys: usize,
        scale: f64,
    ) -> Option<CoreConn> {
        // Resolve via the supervisor binary tree:
        //   1. MARSPOT_CORE_BIN env override (full path or sibling
        //      name — useful in dev / when pointing at an alt core).
        //   2. `~/Library/Caches/marspot/binaries/current/marspot-core`
        //      if a prior silent update has staged one.
        //   3. Sibling of `marspot-shell` (dev / first-run install).
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.current_exe_failed", &format!("{e}"));
                return None;
            }
        };
        let core_bin = self.binaries.resolve_runnable(&exe);

        // Create the bidirectional control socket BEFORE spawn so the
        // child can inherit one end as fd 3.  socketpair(AF_UNIX,
        // SOCK_STREAM) gives us two fds in the parent: parent_fd keeps
        // the shell's side; child_fd gets dup2'd to 3 in pre_exec, then
        // closed in the parent after spawn returns.
        let mut sp = [0i32; 2];
        let r = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr())
        };
        if r != 0 {
            lx_error!(
                "shell.socketpair_failed",
                &format!("{}", std::io::Error::last_os_error())
            );
            return None;
        }
        let parent_fd: RawFd = sp[0];
        let child_fd: RawFd = sp[1];

        // CLOEXEC both ends so the core child never inherits the shell
        // (parent) end of its own control socket: without this the core
        // holds both ends, so fd 3 never sees EOF when the shell dies and
        // the core — plus its whole L3 tree — orphans instead of exiting
        // (a 1 core + N session leak per shell crash/restart). The
        // pre_exec dup2 re-clears CLOEXEC on the child's fd 3 below, so
        // the core still gets its control socket. A pending core spawned
        // during a dual-core update likewise won't inherit the active
        // core's control end.
        for fd in [parent_fd, child_fd] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            }
        }

        lx_info!(
            "shell.core.spawning",
            "spawning marspot-core",
            bin = core_bin.display(),
            front_id = front_id,
            back_id = back_id,
            w = w_phys,
            h = h_phys,
            scale = scale,
            control_fd = DEFAULT_CONTROL_FD
        );
        let mut cmd = Command::new(&core_bin);
        cmd.env(ENV_SURFACE_ID, front_id.to_string())
            .env(ENV_SURFACE_ID_BACK, back_id.to_string())
            .env(ENV_SURFACE_WIDTH, w_phys.to_string())
            .env(ENV_SURFACE_HEIGHT, h_phys.to_string())
            .env(ENV_SURFACE_SCALE, scale.to_string())
            .env(ENV_CONTROL_FD, DEFAULT_CONTROL_FD.to_string());
        // SAFETY: pre_exec runs in the forked child between fork and
        // exec.  Only async-signal-safe libc calls are allowed; we
        // only use dup2/close/fcntl which are all on the AS-safe list.
        unsafe {
            cmd.pre_exec(move || {
                if child_fd != DEFAULT_CONTROL_FD {
                    if libc::dup2(child_fd, DEFAULT_CONTROL_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(child_fd);
                }
                // Strip CLOEXEC so the core sees fd 3 after exec.
                let flags = libc::fcntl(DEFAULT_CONTROL_FD, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(
                        DEFAULT_CONTROL_FD,
                        libc::F_SETFD,
                        flags & !libc::FD_CLOEXEC,
                    );
                }
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                lx_event!(
                    "CORE_SPAWN",
                    "spawned marspot-core",
                    pid = pid,
                    bin = core_bin.display()
                );
                sup_log::log(
                    "CORE_SPAWN",
                    &format!("pid={pid} bin={}", core_bin.display()),
                );
                // Parent no longer needs the child end.
                unsafe { libc::close(child_fd) };
                // Wrap the parent end as a UnixStream we can write
                // frames to from any callback.
                let stream = unsafe { UnixStream::from_raw_fd(parent_fd) };
                let reader_stream = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        lx_error!(
                            "shell.control_stream.try_clone_failed",
                            &format!("{e}")
                        );
                        let mut child = child;
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                };

                // Spawn the reader thread.  Decodes frames into
                // ShellInbox messages, hands them to the main thread
                // via mpsc + `EventProxy::wake`.
                let (tx, rx): (Sender<ShellInbox>, Receiver<ShellInbox>) = mpsc::channel();
                let proxy = self.proxy.clone();
                std::thread::spawn(move || control_reader_loop(reader_stream, tx, proxy));

                // Liveness handshake bookkeeping — a fresh core gets a
                // fresh probe window.  Nonce starts at 0 (bumped to 1
                // before the first ping); this conn's reader owns its
                // own channel, so a previous core's Pong can't alias it.
                let now = Instant::now();
                let conn = CoreConn {
                    child: Some(child),
                    control_tx: Mutex::new(stream),
                    control_rx: rx,
                    hello_acked: false,
                    spawned_at: now,
                    next_ping_at: now + PING_INTERVAL,
                    last_ping_nonce: 0,
                    last_pong_at: now, // freebie until first ping
                };

                // Send HELLO on this core's own socket so it can echo
                // HelloAck — this works whether the conn ends up active
                // or pending.
                conn.send(MsgType::Hello, encode_hello(PROTO_VERSION));
                Some(conn)
            }
            Err(e) => {
                lx_error!("shell.core.spawn_failed", &format!("{e}"));
                unsafe {
                    libc::close(parent_fd);
                    libc::close(child_fd);
                }
                None
            }
        }
    }

    /// Begin a flash-free silent update.  Promote `pending/marspot-core`
    /// to `current/`, then spawn a **pending** core into a *fresh*
    /// IOSurface — WITHOUT touching the active core.  The active core
    /// keeps rendering the surface the user sees; the pending core
    /// rebuilds the same screen off-screen from shelld's bytelog.  When
    /// it proves out (HelloAck + SurfaceReady + probation) the presenter
    /// atomic-swaps to the new surface (`promote_pending_to_active`); on
    /// failure it's killed and the binary rolled back, all unseen.
    ///
    /// Returns `true` if a pending update was started; `false` (no-op)
    /// when there's no pending binary or one is already in flight.
    fn apply_pending_update(&mut self, ctx: &MarspotAppCtx) -> bool {
        if !matches!(self.sup_state, SupervisorState::Idle) || self.pending.is_some() {
            return false;
        }
        // Task C: shell-self-update has precedence over core update —
        // a pending shell binary means the supervisor itself wants to
        // turn over, which implies the renderer probably wants
        // turning over too (the shell + core release together).
        // Detecting + promoting the shell's pending here means a
        // single focus-loss handles both.
        if self.try_apply_shell_self_update(ctx.window_frame_pt()) {
            // We exec'd; this function call's stack frame is gone.
            // Returning here only happens if exec failed.
            return false;
        }
        if !self.binaries.has_pending() {
            return false;
        }
        // Size the pending core's surface pair to the displayed one.
        let (w_px, h_px) = match self.surfaces.as_ref() {
            Some(s) => (s.width(), s.height()),
            None => {
                lx_warn!(
                    "shell.apply_update.no_surface",
                    "no displayed surface to match"
                );
                return false;
            }
        };
        lx_event!("UPDATE_APPLY", "starting dual-core update");
        sup_log::log("UPDATE_APPLY", "promoting pending → current (dual-core)");
        if let Err(e) = self.binaries.promote_pending() {
            lx_event!(
                "UPDATE_FAIL",
                "promote_pending failed; leaving active core untouched",
                error = format!("{e}")
            );
            sup_log::log("UPDATE_FAIL", &format!("promote_pending: {e}"));
            return false;
        }
        // Fresh pair for the pending core — the active core's pair
        // (self.surfaces) is left completely alone, so the user sees no
        // change while the new core warms up.
        let new_pair = match SurfacePair::create(w_px, h_px) {
            Ok(p) => p,
            Err(e) => {
                lx_event!(
                    "UPDATE_FAIL",
                    "pending SurfacePair::create failed",
                    error = format!("{e}")
                );
                sup_log::log("UPDATE_FAIL", &format!("pair create: {e}"));
                self.rollback_binary("pair create failed");
                return false;
            }
        };
        let scale = ctx.scale();
        let (front_id, back_id) = new_pair.ids();
        let conn = match self.spawn_core(front_id, back_id, w_px, h_px, scale) {
            Some(c) => {
                self.record_core_boot();
                c
            }
            None => {
                lx_event!("UPDATE_FAIL", "spawn pending core failed");
                sup_log::log("UPDATE_FAIL", "spawn pending core");
                new_pair.release();
                self.rollback_binary("spawn pending core failed");
                return false;
            }
        };
        self.pending = Some(PendingUpdate {
            conn,
            surfaces: new_pair,
            surface_ready: false,
        });
        // The core doesn't auto-emit SurfaceReady for its initial
        // env-var pair — send a SurfaceAttach to drive it.  Same dims
        // here (no resize); this is purely the handshake trigger that
        // tells the pending core "go render, ack when ready", which is
        // the second promotion gate.
        if let Some(p) = self.pending.as_ref() {
            let (f, b) = p.surfaces.ids();
            p.conn.send(
                MsgType::SurfaceAttach,
                encode_surface_attach(f, b, w_px as f64, h_px as f64, scale),
            );
        }
        self.sup_state = SupervisorState::Probation {
            started_at: std::time::Instant::now(),
        };
        true
    }

    /// Atomic-swap the presenter onto the pending core's surface, retire
    /// the old active core, and promote pending → active.  Both surfaces
    /// carry identical content (same shelld bytelog), so the swap is
    /// invisible.  Called once the pending core has cleared probation.
    fn promote_pending_to_active(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let PendingUpdate {
            conn,
            surfaces: new_pair,
            ..
        } = pending;
        // Point the presenter at the new pair.  The next present()
        // samples it; until then the old pair is still shown.
        if let Some(p) = self.presenter.as_mut() {
            if let Err(e) = p.set_pair(&new_pair.front, &new_pair.back) {
                // Swap failed — keep the active core + its pair, kill
                // the pending core, and roll the binary back.  The user
                // never saw anything change.
                lx_event!(
                    "UPDATE_FAIL",
                    "promote set_pair failed; keeping active core",
                    error = format!("{e}")
                );
                sup_log::log("UPDATE_FAIL", &format!("set_pair: {e}"));
                new_pair.release();
                conn.shutdown();
                self.rollback_binary("set_pair failed");
                self.sup_state = SupervisorState::Idle;
                return;
            }
        }
        // Release the old displayed pair, install the new one.  Any
        // in-flight resize pair is now stale (resize aborts pending
        // updates, so this is belt-and-suspenders) — drop it too.
        if let Some(old) = self.surfaces.take() {
            old.release();
        }
        if let Some(stale) = self.pending_surfaces.take() {
            stale.release();
        }
        self.surfaces = Some(new_pair);
        self.first_frame_ready = true;
        // Retire the old active core; the pending core becomes active.
        self.shutdown_active("retired by promoted pending core (UPDATE_SWAP)");
        self.active = Some(conn);
        // The update is committed — drop the rollback target.
        match self.binaries.finalize_stable() {
            Ok(()) => sup_log::log("UPDATE_STABLE", "dual-core swap; prev/ deleted"),
            Err(e) => {
                lx_event!("FINALIZE_FAIL", "finalize_stable failed", error = format!("{e}"));
                sup_log::log("FINALIZE_FAIL", &format!("{e}"));
            }
        }
        lx_event!(
            "UPDATE_SWAP",
            "dual-core swap complete — pending promoted to active"
        );
        sup_log::log("UPDATE_SWAP", "presenter → new surface; pending → active");
    }

    /// Abort an in-flight pending update: kill the pending core, release
    /// its surface, and roll the binary back.  The active core and its
    /// surface are untouched — the user sees nothing.  No-op if there's
    /// no pending update.
    fn abort_pending_update(&mut self, reason: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let PendingUpdate { conn, surfaces, .. } = pending;
        conn.shutdown();
        surfaces.release();
        lx_event!("UPDATE_ABORT", "aborting pending update", reason = reason);
        sup_log::log("UPDATE_ABORT", reason);
        self.rollback_binary(reason);
    }

    /// Roll `current/marspot-core` back to `prev/` after a failed
    /// update attempt.  The active core is still running the old binary,
    /// so this just realigns the binary tree with reality.
    fn rollback_binary(&self, reason: &str) {
        match self.binaries.rollback_to_prev() {
            Ok(true) => {
                lx_event!("ROLLBACK", "rolled back to prev/", reason = reason);
                sup_log::log("ROLLBACK", "prev/ → current/");
            }
            Ok(false) => {
                lx_event!("ROLLBACK_NOOP", "no prev/ to restore", reason = reason);
                sup_log::log("ROLLBACK_NOOP", "no prev/ to restore");
            }
            Err(e) => {
                lx_event!("ROLLBACK_FAIL", "rollback_to_prev failed", error = format!("{e}"));
                sup_log::log("ROLLBACK_FAIL", &format!("{e}"));
            }
        }
    }

    /// Task C — shell self-update.  Detect a pending shell binary,
    /// promote it into `current/`, and `execv` over ourselves so the
    /// new shell binary takes over the same process slot.  The window
    /// flashes closed → open in ~100 ms; the new shell reconnects to
    /// shelld and reattaches the user's existing sessions so terminal
    /// content survives.
    ///
    /// Returns `true` if exec was attempted (caller's stack is gone
    /// past that point, but Rust can't express it).  Returns `false`
    /// if no pending shell was found *or* if any prep step failed.
    fn try_apply_shell_self_update(&mut self, window_frame_pt: (f64, f64, f64, f64)) -> bool {
        use std::os::unix::process::CommandExt;
        let shell_tree = match supervisor::BinaryTree::for_shell() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if !shell_tree.has_pending() {
            return false;
        }
        lx_event!(
            "SHELL_UPDATE_APPLY",
            "applying pending shell self-update"
        );
        sup_log::log(
            "SHELL_UPDATE_APPLY",
            "promoting pending/marspot-shell → current/",
        );
        if let Err(e) = shell_tree.promote_pending() {
            lx_event!(
                "SHELL_UPDATE_FAIL",
                "shell promote_pending failed",
                error = format!("{e}")
            );
            sup_log::log("SHELL_UPDATE_FAIL", &format!("promote: {e}"));
            return false;
        }
        // Tear down what we can pre-exec so the new shell starts
        // fresh.  Core child gets killed; control socket is dropped;
        // IOSurface is released.  Anything left would be inherited as
        // dangling fds in the new process — clean now.
        self.shutdown_active("shell self-update execv prep");
        if let Some(pending) = self.pending.take() {
            let PendingUpdate { conn, surfaces, .. } = pending;
            conn.shutdown();
            surfaces.release();
        }
        if let Some(s) = self.surfaces.take() {
            s.release();
        }
        if let Some(s) = self.pending_surfaces.take() {
            s.release();
        }
        let target = shell_tree.current();
        if !target.exists() {
            lx_event!(
                "SHELL_UPDATE_FAIL",
                "post-promote current/marspot-shell missing — aborting exec"
            );
            sup_log::log("SHELL_UPDATE_FAIL", "post-promote current missing");
            return false;
        }
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let arg0 = args.first().cloned().unwrap_or_else(|| target.clone().into());
        // Pass the bundle directory so the new shell can fall back
        // there when looking for marspot-core (the updater might
        // have staged only shell, not core).  Computed from our own
        // `current_exe()` parent.
        let bundle_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        // Pass a marker so the new shell logs SHELL_SELF_UPDATE on
        // startup and so the redirect-loop guard doesn't fire (the
        // new binary IS the one we want to run).
        // Hand the live window frame to the successor so its window
        // opens exactly in place (no jump back to the default rect).
        let (fx, fy, fw, fh) = window_frame_pt;
        let err = std::process::Command::new(&target)
            .arg0(arg0)
            .args(args.iter().skip(1))
            .env("MARSPOT_NO_REDIRECT", "1")
            .env("MARSPOT_SHELL_SELF_UPDATE", "1")
            .env("MARSPOT_BUNDLE_DIR", &bundle_dir)
            .env("MARSPOT_RESTORE_FRAME", format!("{fx},{fy},{fw},{fh}"))
            .exec();
        // exec only returns on failure.
        lx_event!(
            "SHELL_UPDATE_FAIL",
            "exec into current/marspot-shell failed",
            target = target.display(),
            error = format!("{err}")
        );
        sup_log::log("SHELL_UPDATE_FAIL", &format!("exec: {err}"));
        false
    }

    /// Resolve which banner (if any) the current shell state wants
    /// to show, and push it into the presenter if it changed.
    ///
    /// Note: a dual-core update on probation shows **no** banner — the
    /// whole point is invisibility, the active core keeps rendering
    /// normally while the pending core warms up off-screen.
    fn refresh_banner(&mut self, ctx: &MarspotAppCtx) {
        let want = if self.auto_restart_disabled {
            Some(BannerKind::UpdateFailed)
        } else if !self.core_alive() && self.surfaces.is_some() {
            // Active core process is gone (just SIGKILL'd or died and we
            // haven't respawned yet).  Show the recovering banner while
            // the gap lasts.
            Some(BannerKind::Recovering)
        } else {
            None
        };
        if want == self.banner_kind {
            return;
        }
        if let Some(p) = self.presenter.as_mut() {
            let scale = ctx.scale();
            if let Err(e) = p.set_banner(want, scale) {
                lx_warn!("shell.set_banner_failed", &format!("{e}"));
                return;
            }
        }
        self.banner_kind = want;
    }

    /// Record a crash event in the rolling window.  Trips
    /// `auto_restart_disabled` if too many have happened recently.
    fn record_crash(&mut self) {
        let now = Instant::now();
        self.crashes.push_back(now);
        while let Some(t) = self.crashes.front() {
            if now.duration_since(*t) > CRASH_WINDOW {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
        sup_log::log(
            "CRASH",
            &format!("count={}/{}", self.crashes.len(), MAX_CRASHES_IN_WINDOW),
        );
        if self.crashes.len() > MAX_CRASHES_IN_WINDOW {
            self.auto_restart_disabled = true;
            lx_event!(
                "BUDGET_EXCEEDED",
                "crash budget exceeded; auto-restart disabled until manual intervention",
                count = self.crashes.len(),
                window_s = CRASH_WINDOW.as_secs()
            );
            sup_log::log(
                "BUDGET_EXCEEDED",
                &format!("count={} window_s={}", self.crashes.len(), CRASH_WINDOW.as_secs()),
            );
        }
    }

    /// Kill the running core (best-effort) and re-spawn from
    /// `current/`.  Used both after a clean detected crash and after
    /// a hang.  Honours `auto_restart_disabled`.
    fn restart_core(&mut self, ctx: &MarspotAppCtx) {
        // Tear down whatever's left.
        self.shutdown_active("supervisor restart_core (crash/hang/respawn)");
        if self.auto_restart_disabled {
            return;
        }
        if let Some(s) = self.surfaces.as_ref() {
            let (front_id, back_id) = s.ids();
            let w_px = s.width();
            let h_px = s.height();
            let scale = ctx.scale();
            self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
            if self.active.is_some() {
                self.record_core_boot();
            }
            // A freshly spawned core attaches its env pair but never
            // emits SurfaceReady spontaneously — drive it.  Initial boot
            // gets the drive for free from the framework's post-`resumed`
            // `resized` callback; a *restart* (crash / hang / boot-race
            // recovery) doesn't.  Without the explicit drive the new
            // core renders into the pair but `first_frame_ready` never
            // flips, so `redraw`/`present` stay gated and the window is
            // black until the user resizes.  We hand it a fresh pair via
            // SurfaceAttach so the handshake mirrors the resize path; the
            // old pair keeps showing its last frame until SurfaceReady
            // for the new pair swaps it in (no black flash on a live
            // crash).
            if self.active.is_some() {
                match SurfacePair::create(w_px, h_px) {
                    Ok(pair) => {
                        if let Some(stale) = self.pending_surfaces.take() {
                            stale.release();
                        }
                        let (f, b) = pair.ids();
                        self.pending_surfaces = Some(pair);
                        self.send(
                            MsgType::SurfaceAttach,
                            encode_surface_attach(
                                f,
                                b,
                                w_px as f64,
                                h_px as f64,
                                scale,
                            ),
                        );
                    }
                    Err(e) => {
                        lx_error!(
                            "shell.restart_handshake.pair_create_failed",
                            &format!("{e}")
                        );
                    }
                }
            }
        }
    }

    /// Periodic check.  Fired from `user_event` (which runs every
    /// 16 ms via the redraw pump).  Responsibilities:
    ///
    ///   1. If the *active* (visible) core died → restart it (and
    ///      abandon any in-flight update).
    ///   2. Drive any in-flight silent update via `poll_pending_update`
    ///      (the probationary core's crash/timeout/ready handling).
    ///   3-5. Active core healthcheck: HELLO timeout, PING, PONG
    ///      deadline — unchanged, but scoped to `active` only.
    ///   6. SIGUSR1 manual trigger; 7. banner refresh.
    fn poll_supervisor(&mut self, ctx: &MarspotAppCtx) {
        // Plugin tick — runs at the supervisor cadence (~250 ms via
        // start_redraw_pump), each plugin further gates by its own
        // tick_interval_ms.  Cheap when no plugin needs to tick.
        // last_plugin_tick is reserved for future "supervisor-level
        // throttle" — MVP doesn't gate here, the registry handles it.
        self.last_plugin_tick = Instant::now();
        self.plugin_registry.tick_all_with(&self.plugin_host);

        // Drain any badge updates plugins queued during the tick and
        // forward to L2 as PaneBadge frames.  No L2 (pre-boot or
        // during a crash gap) → just drop the update; the next tick
        // will push the current mapping again (plugins re-issue every
        // transition, not just once).
        while let Ok(upd) = self.pane_badge_rx.try_recv() {
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::PaneBadge,
                    marspot::shell_proto::encode_pane_badge(
                        upd.shelld_session_id,
                        &upd.text,
                    ),
                );
            }
            // No active core → drop silently.  Plugin re-pushes every
            // tick so the next valid core will pick it up.
        }

        // 1. Active core liveness — the core the user is looking at.
        let active_exited = match self.active.as_mut().and_then(|c| c.child.as_mut()) {
            Some(c) => matches!(c.try_wait(), Ok(Some(_))),
            None => false,
        };
        if active_exited {
            // Reap; restart_core below drops the rest of the conn.
            if let Some(c) = self.active.as_mut() {
                c.child.take();
            }
            self.record_crash();
            // If an update was on probation, the *visible* core just
            // died — abandon the unproven pending core and restore the
            // user's core from current/.
            if self.pending.is_some() {
                lx_event!(
                    "CORE_GONE",
                    "active core died mid-update → aborting pending update"
                );
                self.abort_pending_update("active core exited during pending update");
                self.sup_state = SupervisorState::Idle;
            } else {
                lx_event!(
                    "CORE_GONE",
                    "active core exited unexpectedly → restarting"
                );
            }
            self.restart_core(ctx);
            return;
        }

        // 2. Drive any in-flight silent update.  Crash / HELLO-timeout /
        //    hang → abort + rollback (active untouched);  HelloAck +
        //    SurfaceReady + probation elapsed → atomic swap.  Leaves
        //    sup_state Idle when it resolves.
        self.poll_pending_update();

        // 3. Active HELLO timeout.
        let hello_timed_out = self
            .active
            .as_ref()
            .map(|c| !c.hello_acked && c.spawned_at.elapsed() > HELLO_TIMEOUT)
            .unwrap_or(false);
        if hello_timed_out {
            lx_event!(
                "HELLO_TIMEOUT",
                "core failed to HelloAck → killing",
                timeout_s = HELLO_TIMEOUT.as_secs()
            );
            self.record_crash();
            self.restart_core(ctx);
            return;
        }

        // 4. Time to send the next ping?  Mutate the conn first, then
        // send (which borrows `self` immutably) with the new nonce.
        let now = Instant::now();
        let ping_nonce = self.active.as_mut().and_then(|c| {
            if c.hello_acked && now >= c.next_ping_at {
                c.last_ping_nonce = c.last_ping_nonce.wrapping_add(1);
                c.next_ping_at = now + PING_INTERVAL;
                Some(c.last_ping_nonce)
            } else {
                None
            }
        });
        if let Some(nonce) = ping_nonce {
            self.send(MsgType::Ping, encode_ping(nonce));
        }

        // 5. Pong deadline → hung.
        let pong_timed_out = self
            .active
            .as_ref()
            .map(|c| c.hello_acked && now.duration_since(c.last_pong_at) > PONG_DEADLINE)
            .unwrap_or(false);
        if pong_timed_out {
            lx_event!(
                "PONG_TIMEOUT",
                "no PONG → core hung; SIGKILL + restart",
                deadline_s = PONG_DEADLINE.as_secs()
            );
            self.record_crash();
            self.restart_core(ctx);
        }

        // 7. Manual update trigger via SIGUSR1.  Lets a CLI invoke
        // `kill -USR1 $(pgrep marspot-shell)` to apply a staged
        // update on demand instead of waiting for focus-loss.
        if SIGUSR1_FLAG.swap(false, Ordering::AcqRel) {
            sup_log::log("SIGUSR1", "manual update trigger");
            if matches!(self.sup_state, SupervisorState::Idle) {
                self.apply_pending_update(ctx);
            } else {
                lx_warn!(
                    "shell.sigusr1.ignored_not_idle",
                    "SIGUSR1 ignored — supervisor not idle"
                );
            }
        }

        // 6. Recompute the banner once per tick — whatever
        // transition happened above, the visible banner should
        // reflect it.
        self.refresh_banner(ctx);
    }

    /// Drive an in-flight silent update (`self.pending`).  No-op when
    /// there's no pending update.  Resolves to either an atomic swap
    /// (`promote_pending_to_active`) or an abort + rollback, and in
    /// both cases returns `sup_state` to `Idle`.
    ///
    /// The pending core gets the same liveness gauntlet as the active
    /// one — child-exit, HELLO timeout, and PONG-deadline (we ping it
    /// during probation) — so a binary that boots but is broken or
    /// hangs is caught and rolled back *before* it's ever shown.
    fn poll_pending_update(&mut self) {
        if self.pending.is_none() {
            return;
        }
        let now = Instant::now();

        // a. Pending child exited, or was killed by a HELLO mismatch
        //    (child taken → None) → abort.
        let pending_dead = match self.pending.as_mut() {
            Some(p) => match p.conn.child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => true, // child already gone (mismatch kill)
            },
            None => false,
        };
        if pending_dead {
            // Note: a pending-core death does NOT count toward the
            // active core's crash budget — it never touched the user.
            // It just fails this update; the rolled-back binary won't be
            // retried until the updater re-stages pending/.
            self.abort_pending_update("pending core exited during probation");
            self.sup_state = SupervisorState::Idle;
            return;
        }

        // b. Pending HELLO timeout.
        let hello_timeout = self
            .pending
            .as_ref()
            .map(|p| !p.conn.hello_acked && p.conn.spawned_at.elapsed() > HELLO_TIMEOUT)
            .unwrap_or(false);
        if hello_timeout {
            self.abort_pending_update("pending core HELLO timeout");
            self.sup_state = SupervisorState::Idle;
            return;
        }

        // c. Pending hung after handshake (PONG deadline).
        let pong_timeout = self
            .pending
            .as_ref()
            .map(|p| p.conn.hello_acked && now.duration_since(p.conn.last_pong_at) > PONG_DEADLINE)
            .unwrap_or(false);
        if pong_timeout {
            self.abort_pending_update("pending core PONG timeout (hung)");
            self.sup_state = SupervisorState::Idle;
            return;
        }

        // d. Ping the pending core so (c) can catch a hang.
        let ping_nonce = self.pending.as_mut().and_then(|p| {
            if p.conn.hello_acked && now >= p.conn.next_ping_at {
                p.conn.last_ping_nonce = p.conn.last_ping_nonce.wrapping_add(1);
                p.conn.next_ping_at = now + PING_INTERVAL;
                Some(p.conn.last_ping_nonce)
            } else {
                None
            }
        });
        if let Some(nonce) = ping_nonce {
            if let Some(p) = self.pending.as_ref() {
                p.conn.send(MsgType::Ping, encode_ping(nonce));
            }
        }

        // e. Ready to promote?  HelloAck + SurfaceReady + probation
        //    elapsed → atomic swap.
        let ready = self
            .pending
            .as_ref()
            .map(|p| p.conn.hello_acked && p.surface_ready)
            .unwrap_or(false)
            && self.sup_state.probation_elapsed();
        if ready {
            self.promote_pending_to_active();
            self.sup_state = SupervisorState::Idle;
        }
    }

    fn start_redraw_pump(&mut self) {
        if self.redraw_thread_started {
            return;
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(REDRAW_INTERVAL_MS));
            proxy.wake();
        });
        self.redraw_thread_started = true;
    }

    /// Honour a `SurfaceReady(id)` ack from the core.  Two cases:
    ///
    /// 1. **Steady-state per-frame ack** — id belongs to the *live*
    ///    pair: the core just finished writing that half, the other
    ///    half is now the "back".  Flip `current_idx` and present.
    /// 2. **Pair handshake** — id belongs to the *pending* pair
    ///    (resize / restart created a new pair, sent SurfaceAttach,
    ///    core acked).  Install pending as the live pair, point the
    ///    presenter at it, and flip to the acked id.
    ///
    /// IDs that don't match either are late acks for a retired pair
    /// (replaced by a newer resize before the core got there) —
    /// silently dropped.
    fn on_surface_ready(&mut self, id: u32) {
        let live_ids = self.surfaces.as_ref().map(|p| p.ids());
        let pending_ids = self.pending_surfaces.as_ref().map(|p| p.ids());
        let in_live = live_ids
            .map(|(f, b)| f == id || b == id)
            .unwrap_or(false);
        let in_pending = pending_ids
            .map(|(f, b)| f == id || b == id)
            .unwrap_or(false);
        if in_live && !in_pending {
            // Per-frame ack: just point the presenter at the just-
            // completed half.  Sampled (1/8 ≈ render.frame's cadence
            // so the two streams sync at MARSPOT_LOG=debug) — the
            // live-pair-swap path is the dominant rate on a busy
            // display and would otherwise drown the log.
            lx_debug_sampled!(
                "shell.surface.swap",
                8,
                "per-frame SurfaceReady ack",
                id = id
            );
            if let Some(p) = self.presenter.as_mut() {
                if !p.swap_to_id(id) {
                    // Presenter and shell pair-ids disagree — should
                    // not happen, but log if it ever does.
                    lx_warn!(
                        "shell.presenter.swap_to_id_unknown",
                        "presenter rejected SurfaceReady id",
                        id = id
                    );
                    return;
                }
            }
            // First-ever SurfaceReady on the boot pair flips the
            // first_frame_ready gate so `redraw()` finally presents.
            // Without this the gate stays false for the entire run
            // (it only flipped on the legacy v=1 handshake path).
            self.first_frame_ready = true;
            self.frame_pending = true;
            return;
        }
        if !in_pending {
            lx_warn!(
                "shell.surface_ready.id_unknown",
                "SurfaceReady ignored — id does not match live or pending pair",
                id = id,
                live = format!("{live_ids:?}"),
                pending = format!("{pending_ids:?}")
            );
            return;
        }
        // Handshake ack: install pending as live.
        let new_pair = match self.pending_surfaces.take() {
            Some(p) => p,
            None => return,
        };
        if let Some(p) = self.presenter.as_mut() {
            if let Err(e) = p.set_pair(&new_pair.front, &new_pair.back) {
                lx_error!("shell.set_pair_failed", &format!("{e}"));
                new_pair.release();
                return;
            }
            // The acked id is the one the core just wrote — point at it.
            p.swap_to_id(id);
        }
        if let Some(old) = self.surfaces.take() {
            old.release();
        }
        lx_info!(
            "shell.presenter.pair_swapped",
            "presenter now displaying new pair",
            front = new_pair.front.id(),
            back = new_pair.back.id(),
            acked = id
        );
        self.surfaces = Some(new_pair);
        self.first_frame_ready = true;
        self.frame_pending = true;
    }

    /// Route a frame from the **active** core — it drives what's on
    /// screen.  `SurfaceReady` here is a resize handoff (same core, new
    /// size); the dual-core update path uses `handle_pending_msg`.
    fn handle_active_msg(&mut self, msg: ShellInbox, ctx: &MarspotAppCtx) {
        match msg {
            ShellInbox::SurfaceReady(id) => self.on_surface_ready(id),
            ShellInbox::HelloAck(v) => {
                if v == PROTO_VERSION {
                    if let Some(c) = self.active.as_mut() {
                        c.hello_acked = true;
                    }
                    lx_event!("HELLO_ACK", "core handshake OK", v = v);
                    sup_log::log("HELLO_ACK", &format!("v={v}"));
                } else {
                    lx_event!(
                        "HELLO_MISMATCH",
                        "HelloAck disagrees with shell version; killing core",
                        core_v = v,
                        shell_v = PROTO_VERSION
                    );
                    sup_log::log("HELLO_MISMATCH", &format!("core_v={v} shell_v={PROTO_VERSION}"));
                    // Kill the child but keep the conn — `spawned_at` and
                    // `hello_acked == false` stay, so the HELLO timeout in
                    // poll_supervisor respawns it.
                    if let Some(c) = self.active.as_mut() {
                        if let Some(mut child) = c.child.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
            ShellInbox::Pong(nonce) => {
                if let Some(c) = self.active.as_mut() {
                    if nonce == c.last_ping_nonce {
                        c.last_pong_at = Instant::now();
                    }
                }
            }
            ShellInbox::CaretRect(rect) => {
                ctx.set_caret_rect_phys(rect);
            }
            // Legacy v=1 wake from the core's pre-A2-A4 single-
            // surface path: the core renders into ENV_SURFACE_ID
            // directly and pokes us with an empty FrameRendered.
            // We never get a SurfaceReady from such a core (it only
            // emits one on Resize), so we ALSO flip first_frame_ready
            // here — otherwise a fresh shell paired with a rolled-back
            // legacy core (`current/marspot-core` was reverted after
            // a probation-abort while the shell self-update succeeded)
            // would gate `redraw()` forever and paint black.
            // A v=2 core never sends this, so it's pure tolerance.
            ShellInbox::FrameRendered => {
                self.first_frame_ready = true;
                self.frame_pending = true;
            }
        }
    }

    /// Route a frame from the **pending** (probationary) core.  It only
    /// reports progress toward promotion — it isn't displayed, so it
    /// receives no input and its caret/resize frames are irrelevant.
    fn handle_pending_msg(&mut self, msg: ShellInbox) {
        match msg {
            ShellInbox::SurfaceReady(id) => {
                // The pending core has painted one half of its pair —
                // one of the two promotion preconditions.
                if let Some(p) = self.pending.as_mut() {
                    let (f, b) = p.surfaces.ids();
                    if f == id || b == id {
                        p.surface_ready = true;
                        lx_event!(
                            "PENDING_SURFACE_READY",
                            "pending core SurfaceReady",
                            id = id
                        );
                        sup_log::log("PENDING_SURFACE_READY", &format!("id={id}"));
                    }
                }
            }
            ShellInbox::HelloAck(v) => {
                if v == PROTO_VERSION {
                    if let Some(p) = self.pending.as_mut() {
                        p.conn.hello_acked = true;
                    }
                    lx_event!("PENDING_HELLO_ACK", "pending core HelloAck", v = v);
                    sup_log::log("PENDING_HELLO_ACK", &format!("v={v}"));
                } else {
                    // Version mismatch — kill the pending child; the next
                    // poll_pending_update tick sees child==None and aborts.
                    lx_event!(
                        "PENDING_HELLO_MISMATCH",
                        "pending core HelloAck version mismatch",
                        core_v = v,
                        shell_v = PROTO_VERSION
                    );
                    sup_log::log("PENDING_HELLO_MISMATCH", &format!("core_v={v}"));
                    if let Some(p) = self.pending.as_mut() {
                        if let Some(mut child) = p.conn.child.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
            ShellInbox::Pong(nonce) => {
                if let Some(p) = self.pending.as_mut() {
                    if nonce == p.conn.last_ping_nonce {
                        p.conn.last_pong_at = Instant::now();
                    }
                }
            }
            // Pending core isn't displayed — its caret + frame pokes are
            // irrelevant (it renders to its own off-screen surface).
            ShellInbox::CaretRect(_) | ShellInbox::FrameRendered => {}
        }
    }
}

impl MarspotApp for ShellApp {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;

        // RFC-001 plugin bootstrap: register first-party plugins,
        // run init then start.  Plugins are passive until the core
        // has booted + sessions attached, so start is fine right
        // here — plugin tick is what drives per-pane work later.
        if self.plugin_registry.is_empty() {
            self.plugin_registry
                .register(Box::new(plugins::claudecode::ClaudecodePlugin::new()));
            self.plugin_registry.init_all_with(&self.plugin_host);
            self.plugin_registry.start_all_with(&self.plugin_host);
        }

        let pair = match SurfacePair::create(w_px, h_px) {
            Ok(p) => p,
            Err(e) => {
                lx_error!(
                    "shell.resumed.pair_create_failed",
                    &format!("{e}"),
                    w = w_px,
                    h = h_px
                );
                ctx.exit();
                return;
            }
        };

        let presenter = match ShellPresenter::new(
            ctx.ns_view(),
            scale as f32,
            &pair.front,
            &pair.back,
        ) {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.resumed.presenter_new_failed", &format!("{e}"));
                ctx.exit();
                return;
            }
        };

        let (front_id, back_id) = pair.ids();
        self.surfaces = Some(pair);
        self.presenter = Some(presenter);

        self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
        if self.active.is_some() {
            self.record_core_boot();
        }
        self.start_redraw_pump();
        ctx.request_redraw();
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        // Drain both cores' inboxes — each reader thread calls
        // `proxy.wake()` after a push, so at least one message is ready.
        // Active and pending get separate routing: the active core
        // drives what's on screen (resize swap, caret, healthcheck);
        // the pending core only reports its probation progress.
        let active_inbox: Vec<ShellInbox> = match self.active.as_ref() {
            Some(conn) => conn.control_rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in active_inbox {
            self.handle_active_msg(msg, ctx);
        }
        let pending_inbox: Vec<ShellInbox> = match self.pending.as_ref() {
            Some(p) => p.conn.control_rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in pending_inbox {
            self.handle_pending_msg(msg);
        }
        self.poll_supervisor(ctx);
        ctx.request_redraw();
    }

    fn key_event(&mut self, _ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
        // Forensic anchor for "key X did the wrong thing" reports.
        // We log the LogicalKey and the modifier byte; the actual PTY
        // bytes are encoded downstream in L3 (input_core::key_event_to_bytes)
        // and the existing `~/.marspot-trace` hook keeps capturing them
        // if it ever existed.  This line being at DEBUG means production
        // (default INFO) pays nothing; `MARSPOT_LOG_SHELL=debug` opens
        // the floodgate when investigating.  Mod byte format mirrors
        // shell_proto::struct_to_mods_byte: shift=1 ctrl=2 alt=4 super=8.
        lx_debug!(
            "input.key",
            "key event forwarded to core",
            logical = format!("{:?}", event.logical),
            mods_byte = struct_to_mods_byte(mods),
            state = format!("{:?}", event.state)
        );
        let wire = event_to_wire(&event, mods);
        self.send(MsgType::KeyEvent, encode_key_event(&wire));
    }

    fn mouse_down(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        self.send(
            MsgType::MouseDown,
            encode_mouse(x, y, struct_to_mods_byte(mods)),
        );
    }

    fn mouse_drag(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64) {
        // No modifier info on drag — pass zero; the renderer doesn't
        // currently need mods for drag-extend selection.
        self.send(MsgType::MouseDrag, encode_mouse(x, y, 0));
    }

    fn mouse_up(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64) {
        self.send(MsgType::MouseUp, encode_mouse(x, y, 0));
    }

    fn scroll(&mut self, _ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise));
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // A pending update was sized to the old window; promoting it now
        // would show a stale-sized surface.  Abort it (rolls the binary
        // back) — the update retries on the next focus-loss at the new
        // size.  Only when the dimensions REALLY change, though: AppKit
        // fires `resized` on its own during the post-execv transition
        // (same dims, no real change) and aborting there throws away a
        // perfectly good in-flight update with no user-visible signal.
        // 2026-06-15 production incident: a same-size resized() canned
        // the dual-core swap, leaving NEW shell + OLD core paired.
        if let Some(p) = self.pending.as_ref() {
            let (pw, ph) = (p.surfaces.width(), p.surfaces.height());
            let real_resize = (pw as f64 - w_phys).abs() > 0.5
                || (ph as f64 - h_phys).abs() > 0.5;
            if real_resize {
                self.abort_pending_update("window resized during probation");
                self.sup_state = SupervisorState::Idle;
            }
        }
        if let Some(p) = self.presenter.as_mut() {
            p.set_drawable_size(w_phys, h_phys);
            // Present *synchronously* inside the resize callback so
            // our drawable lands in the SAME CATransaction AppKit is
            // about to commit for the window-bounds change.  Coupled
            // with `setPresentsWithTransaction(true)` on the layer
            // this gives Sublime-style frame-perfect resize — the
            // window edge and the drawable contents move together,
            // no inter-frame drift.
            if self.first_frame_ready {
                p.present();
            }
        }
        // Fire the IOSurface handoff *immediately* (no debounce).
        // With presents-with-transaction the swap between old and new
        // IOSurface lands in the same CATransaction as the window
        // resize, so the visual stays stable while the terminal grid
        // actually reflows.  Without this, the pane grid never gets
        // SIGWINCH during a drag and lines wrap at the old column
        // count — what the user observed as "换行没跟上".
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;
        match SurfacePair::create(w_px, h_px) {
            Ok(pair) => {
                if let Some(stale) = self.pending_surfaces.take() {
                    stale.release();
                }
                let (f, b) = pair.ids();
                self.pending_surfaces = Some(pair);
                self.send(
                    MsgType::SurfaceAttach,
                    encode_surface_attach(f, b, w_phys, h_phys, scale),
                );
            }
            Err(e) => {
                lx_error!("shell.resize.pair_create_failed", &format!("{e}"));
            }
        }
        ctx.request_redraw();
    }

    fn focused(&mut self, ctx: &MarspotAppCtx, focused: bool) {
        self.send(MsgType::Focus, encode_focus(focused));
        // Silent-update trigger: the user just left marspot's window
        // (cmd-tab, click on another app, minimise).  If a pending
        // binary is staged in `binaries/pending/`, this is the
        // cheapest moment to swap — they're not watching us repaint.
        // The shell window stays put through the swap, the new core
        // attaches to the same IOSurface and shelld session, so when
        // they come back they see the same content rendered by the
        // new version's renderer.
        //
        // Skipped during probation: we don't want to chain updates
        // before knowing if the last one was healthy.
        if !focused && matches!(self.sup_state, SupervisorState::Idle) {
            if std::env::var_os("MARSPOT_MANUAL_UPDATE_ONLY").is_none() {
                self.apply_pending_update(ctx);
            }
        }
    }

    fn ime_preedit_changed(&mut self, _ctx: &MarspotAppCtx, text: &str) {
        self.send(MsgType::Preedit, encode_preedit(text));
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        // RFC-001: clean plugin shutdown FIRST, so plugins releasing
        // host resources (notifications, fs watchers) don't race
        // against the rest of the teardown.
        self.plugin_registry.stop_all_with(&self.plugin_host);

        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.shutdown_active("window close_requested");
        // Tear down an in-flight update too (no rollback — we're
        // exiting, not failing an update).
        if let Some(pending) = self.pending.take() {
            let PendingUpdate { conn, surfaces, .. } = pending;
            conn.shutdown();
            surfaces.release();
        }
        if let Some(pair) = self.surfaces.take() {
            pair.release();
        }
        if let Some(stale) = self.pending_surfaces.take() {
            stale.release();
        }
        ctx.exit();
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        // Hold off until the core has written real content.  Without
        // this gate the user sees an uninitialised IOSurface for
        // ~50-100 ms at startup, then a hard snap to content — reads
        // as a black-then-content flash.
        if !self.first_frame_ready {
            return;
        }
        // Gate the present on whether a fresh frame is actually
        // pending.  See `frame_pending` field doc — the safety-net
        // 250 ms timer wakes the redraw callback unconditionally,
        // but sampling the IOSurface from here on every tick races
        // against core's mid-render state and produces the flash.
        // True safety net: if it's been >1 s since the last present
        // (real "core died" case where no future poke is coming),
        // force a present anyway so the window doesn't appear frozen.
        let now = Instant::now();
        let stale = self
            .last_present_at
            .map(|t| now.duration_since(t) > Duration::from_secs(1))
            .unwrap_or(true);
        if !self.frame_pending && !stale {
            return;
        }
        if let Some(p) = self.presenter.as_mut() {
            p.present();
            self.frame_pending = false;
            self.last_present_at = Some(now);
        }
    }
}

fn control_reader_loop(mut stream: UnixStream, tx: Sender<ShellInbox>, proxy: EventProxy) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(None) => return,
            Ok(Some(frame)) => {
                let msg = match frame.msg_type {
                    MsgType::SurfaceReady => {
                        decode_surface_ready(&frame.payload).ok().map(ShellInbox::SurfaceReady)
                    }
                    MsgType::HelloAck => {
                        decode_hello_ack(&frame.payload).ok().map(ShellInbox::HelloAck)
                    }
                    MsgType::Pong => decode_pong(&frame.payload).ok().map(ShellInbox::Pong),
                    MsgType::CaretRect => decode_caret_rect(&frame.payload)
                        .ok()
                        .map(ShellInbox::CaretRect),
                    // Empty-payload wake: a fresh frame is in the IOSurface.
                    // Mapping it to Some(..) is what makes `proxy.wake()`
                    // fire below → user_event → present.
                    MsgType::FrameRendered => Some(ShellInbox::FrameRendered),
                    // Unknown frames are ignored — keeps forward
                    // compatibility while the protocol grows.
                    _ => None,
                };
                if let Some(m) = msg {
                    if tx.send(m).is_err() {
                        return;
                    }
                    proxy.wake();
                }
            }
            Err(e) => {
                lx_error!("shell.control_reader.error", &format!("{e}"));
                return;
            }
        }
    }
}

fn main() {
    // Make `println!` to a closed pipe (e.g. `marspot-shell --status |
    // head`) exit cleanly with the standard EPIPE convention instead
    // of panicking with a Rust backtrace.  SIG_DFL on macOS for SIGPIPE
    // terminates the process — that's exactly the Unix contract.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Land every supervisor event (via sup_log::log) + future shell-side
    // structured events into the shared marspot.log stream with
    // component="shell". Cheap, idempotent across self-exec restarts.
    marspot::logx::init("shell");

    // Rollback subcommands dispatch BEFORE the redirect: when
    // current/ is broken, exec'ing into it would eat the command.
    match std::env::args().nth(1).as_deref() {
        Some("--rollback-shell") => std::process::exit(cmd_rollback("shell")),
        Some("--rollback-core") => std::process::exit(cmd_rollback("core")),
        _ => {}
    }

    // Bundle binary check: if `binaries/current/marspot-shell` exists
    // and points at a different file than us, re-exec into it.  This
    // is what lets a silent shell update land — the bundle's
    // MacOS/marspot-shell hands off to the current/ slot.
    maybe_redirect_to_current_shell();
    if std::env::var_os("MARSPOT_SHELL_SELF_UPDATE").is_some() {
        sup_log::log("SHELL_SELF_UPDATE", "new shell exec'd from current/");
    }

    // Dispatch CLI subcommands before we touch AppKit / start a
    // window.  --status / --trigger run against an already-running
    // shell, so they must not themselves take over the run loop.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            print_version();
            return;
        }
        Some("--status") => {
            print_status();
            return;
        }
        Some("--trigger") => {
            let code = cmd_trigger();
            std::process::exit(code);
        }
        Some("--help") | Some("-h") => {
            println!(
"marspot-shell — supervisor for the marspot terminal.\n\
\n\
Usage:\n\
  marspot-shell                Start the supervisor (window + core).\n\
  marspot-shell --version      Print version / git / build info.\n\
  marspot-shell --status       Summarise state from supervisor.log + live PIDs.\n\
  marspot-shell --trigger      Apply a staged pending update on a running shell\n\
                               (sends SIGUSR1 to the supervisor process).\n\
  marspot-shell --rollback-shell   Quarantine current/marspot-shell, restore prev/.\n\
  marspot-shell --rollback-core    Quarantine current/marspot-core, restore prev/.\n\
                               Both run offline — restart Marspot afterwards.\n"
            );
            return;
        }
        _ => {}
    }

    lx_event!(
        "STARTUP",
        "marspot-shell starting",
        version_shell = env!("MARSPOT_VERSION_SHELL"),
        version_core = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        build_ts = option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        pid = std::process::id()
    );
    sup_log::log(
        "STARTUP",
        &format!(
            "shell={} core={} git={} pid={}",
            env!("MARSPOT_VERSION_SHELL"),
            env!("MARSPOT_VERSION_CORE"),
            option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
            std::process::id()
        ),
    );
    install_sigusr1_handler();
    // Record our pid for this state dir so `--trigger` / dev-push /
    // install-local signal THIS shell, not whichever marspot-shell
    // `ps` lists first.  execv on self-update keeps the same pid, so
    // the file stays valid across a shell swap.
    write_shell_pid();
    // Spawn the silent-update poller.  It runs forever in the
    // background, downloads new `marspot-core` releases, drops them
    // into `binaries/pending/`.  The supervisor here picks them up on
    // the next focus-loss trigger.  Returns a flag we don't currently
    // consult (Step 5's `BinaryTree::has_pending` is the source of
    // truth); kept alive for the eventual UI affordance.
    let _update_flag = marspot::updater::spawn(env!("MARSPOT_VERSION_CORE").to_string());

    // Frame restore: the predecessor shell (self-update execv) hands
    // its exact window frame over via env so this process reopens in
    // place instead of jumping to the default rect.  Consumed here —
    // removed from our env so spawned children (core) don't carry it.
    let restore_frame = std::env::var("MARSPOT_RESTORE_FRAME").ok().and_then(|s| {
        std::env::remove_var("MARSPOT_RESTORE_FRAME");
        let v: Vec<f64> = s.split(',').filter_map(|p| p.parse().ok()).collect();
        match v[..] {
            [x, y, w, h] if w > 0.0 && h > 0.0 => Some((x, y, w, h)),
            _ => None,
        }
    });
    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: DEFAULT_W_PT,
        height_logical: DEFAULT_H_PT,
        frame_pt: restore_frame,
        // Terminal-black from the first paint: a core (L2) swap that
        // briefly uncovers the layer must show steady black, never a
        // lighter chrome flash.
        bg: marspot::font_cache::BG,
    };
    let proxy = EventProxy::new();
    let app = ShellApp::new(proxy.clone());
    run_app(app, proxy, attrs);
}
