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
    decode_caret_rect, decode_hello_ack, decode_pong, decode_surface_ready, encode_file_drop,
    encode_focus, encode_hello, encode_key_event, encode_mouse, encode_ping, encode_preedit,
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
    /// L2 → L1: user clicked the active prefix of a pane badge on the
    /// pane backing this shelld session.  Dispatched to every loaded
    /// plugin so whichever set the badge can react (claudecode → cycle
    /// the next profile and rerun `claudeN --resume <uuid>`).
    PaneBadgeClicked(u64),
    /// L2 → L1: a keystroke arrived on a pane held by a LOCK_KEYS
    /// PaneSession.  Routed to the matching session's on_user_key.
    PaneSessionKey(u64, marspot::shell_proto::WireKeyEvent),
    /// L2 → L1: user pressed Esc 3× in 5 s; force-end the session
    /// regardless of plugin opinion.
    PaneSessionUserEscape(u64),
    /// L2 → L1: user clicked the toolbar's dev-panel toggle icon.
    /// L1 owns the dev panel's NSWindow visibility; the click here
    /// just flips that bit and the redraw cycle picks it up.
    DevPanelToggle,
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
    /// and liveness-handshake state aggregated into `CoreConn`.
    /// `None` before the first spawn and in the brief gap between a
    /// crash (or single-core swap retirement) and the respawn.
    active: Option<CoreConn>,
    /// True after the core has confirmed at least one SurfaceReady.
    /// Until then `redraw` skips `present()` so the user sees the
    /// NSWindow's BG colour (font_cache::BG) instead of an unfilled
    /// black IOSurface — kills the cold-start flash.
    first_frame_ready: bool,
    /// F3+6.1 — last saved (x, y, w, h, display_id) so per-frame
    /// `resized` / `moved` callbacks dedup: only the saves where the
    /// frame actually changed since the last bin write actually fire
    /// I/O.  Eliminates the live-resize 60+ writes/s storm.
    last_saved_window: Option<(f64, f64, f64, f64, u32)>,
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
    /// Same shape as `pane_badge_rx` but for plugin-set pane titles.
    pane_title_rx: std::sync::mpsc::Receiver<plugins::host::PaneTitleUpdate>,
    /// Receiver for `begin_pane_session` requests.
    pane_session_begin_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneSessionBeginRequest>,
    /// Receiver for cc-plugin inject-input requests; drained each tick
    /// and forwarded to the active core as `MsgType::InjectInput` frames.
    inject_input_rx: std::sync::mpsc::Receiver<plugins::host::InjectInputRequest>,
    /// Sender clone of the badge channel — held so PaneSession host
    /// helpers can push set_badge updates without re-importing the
    /// channel from inside ShellApp methods.
    pane_badge_tx_clone: std::sync::mpsc::Sender<plugins::host::PaneBadgeUpdate>,
    pane_title_tx_clone: std::sync::mpsc::Sender<plugins::host::PaneTitleUpdate>,
    /// Active PaneSessions held by L1 plugins, keyed by shelld
    /// session_id.  At most one per pane.
    active_pane_sessions:
        std::collections::HashMap<u64, ActivePaneSession>,
    /// UI-system dev panel state.  L1 owns this because L1 hosts the
    /// dev panel's independent NSWindow (built in `run_app` →
    /// `dev_window::ensure_built`).  L2 sends `DevPanelToggle`
    /// wire frames on icon click; L1 flips `visible` and the redraw
    /// path drives the AppKit show/hide.
    dev_panel: marspot::ui::components::DevPanelState,
    /// Last persisted dev-window geometry, used to dedup writes the
    /// same way `last_saved_window` does for the main window.
    last_saved_dev_window:
        Option<(f64, f64, f64, f64, u32, bool)>,
    /// Last observed `marspot::ui::theme::version()` value.  When
    /// the theme is swapped via `set_current()`, the framework bumps
    /// this counter; we compare each `redraw()` to invalidate cached
    /// state and trigger an additional repaint.
    last_theme_version: u64,
    /// Dev panel content has changed and the next `redraw()` should
    /// re-render its NSWindow.  Set on visibility flip / click /
    /// scroll / window resize / theme swap;  cleared after render.
    /// Without this every main-window redraw (driven by ~60fps PTY
    /// traffic) was re-laying out ~580 view-tree nodes, eating ~10%+
    /// CPU continuously.  See 0.6.26 release note.
    dev_panel_dirty: bool,
}

/// Bundle: the plugin's session object + the metadata we need to log
/// + cleanly tear it down (plugin name for log namespace).
struct ActivePaneSession {
    session: Box<dyn plugins::PaneSession>,
    plugin_name: &'static str,
}

/// Concrete `PaneSessionHost` constructed per-dispatch; lives only
/// for the duration of one plugin callback.  set_badge → channel,
/// end → flips a `Cell` the main-loop checks after the callback
/// returns.  log → logx via the plugin namespace.
struct ConcretePaneSessionHost<'a> {
    sid: u64,
    plugin_name: &'static str,
    badge_tx: &'a std::sync::mpsc::Sender<plugins::host::PaneBadgeUpdate>,
    title_tx: &'a std::sync::mpsc::Sender<plugins::host::PaneTitleUpdate>,
    end_requested: &'a std::cell::Cell<bool>,
}

impl<'a> plugins::PaneSessionHost for ConcretePaneSessionHost<'a> {
    fn shelld_session_id(&self) -> u64 {
        self.sid
    }
    fn end(&self) {
        self.end_requested.set(true);
    }
    fn set_badge(&self, text: &str) {
        let _ = self.badge_tx.send(plugins::host::PaneBadgeUpdate {
            shelld_session_id: self.sid,
            text: text.to_string(),
        });
    }
    fn set_pane_title(&self, text: &str) {
        let _ = self.title_tx.send(plugins::host::PaneTitleUpdate {
            shelld_session_id: self.sid,
            text: text.to_string(),
        });
    }
    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        let composed = format!("plugin.{}.{}", self.plugin_name, tag);
        match level {
            plugins::LogLevel::Debug => marspot::lx_debug!(&*composed, msg),
            plugins::LogLevel::Info => marspot::lx_info!(&*composed, msg),
            plugins::LogLevel::Warn => marspot::lx_warn!(&*composed, msg),
            plugins::LogLevel::Error => marspot::lx_error!(&*composed, msg),
        }
    }
}

impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        let (pane_badge_tx, pane_badge_rx) = std::sync::mpsc::channel();
        let pane_badge_tx_clone = pane_badge_tx.clone();
        let (pane_title_tx, pane_title_rx) = std::sync::mpsc::channel();
        let pane_title_tx_clone = pane_title_tx.clone();
        let (pane_session_begin_tx, pane_session_begin_rx) = std::sync::mpsc::channel();
        let (inject_input_tx, inject_input_rx) = std::sync::mpsc::channel();
        Self {
            proxy,
            surfaces: None,
            pending_surfaces: None,
            presenter: None,
            active: None,
            first_frame_ready: false,
            last_saved_window: None,
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
                h.attach_pane_title_tx(pane_title_tx);
                h.attach_pane_session_begin_tx(pane_session_begin_tx);
                h.attach_inject_input_tx(inject_input_tx);
                h
            },
            plugin_registry: PluginRegistry::new(),
            last_plugin_tick: Instant::now() - Duration::from_secs(1),
            pane_badge_rx,
            pane_title_rx,
            pane_session_begin_rx,
            inject_input_rx,
            pane_badge_tx_clone,
            pane_title_tx_clone,
            active_pane_sessions: std::collections::HashMap::new(),
            dev_panel: marspot::ui::components::DevPanelState::default(),
            last_saved_dev_window: None,
            last_theme_version: marspot::ui::theme::version(),
            dev_panel_dirty: true,   // 0.6.26 — render once on first frame
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

    /// Apply a pending L2 update via **single-core in-place swap**.
    ///
    /// History: this used to run a dual-core probation pattern —
    /// active + pending L2 in parallel for 30 s, presenter atom-swaps
    /// to pending if it survives probation.  That assumed pending +
    /// active could share L3 control connections (the L4-shelld model
    /// where shelld was a multi-consumer broker).  RFC-003 made L3
    /// single-client: as soon as pending core's UDS hello reaches L3,
    /// L3 closes the previous client (= active core).  Result for the
    /// full probation window: active core has dead L3 control sockets,
    /// L1 still routes input to active, every keystroke drops on the
    /// floor.  The user sees panes (active still reads grid shm) but
    /// can't type.
    ///
    /// L3 self-execv + state.bin reattach (RFC-003) means each L3
    /// survives any L2 swap on its own.  We don't need probation as a
    /// warm-up net — just swap atomically: kill active, spawn new from
    /// promoted current/, the new core reattaches via registry.  The
    /// only visible cost is the ~200-500 ms gap from old-core-down to
    /// new-core's first SurfaceReady; the IOSurface pair is reused, so
    /// no presenter handshake is required.
    ///
    /// Returns `true` if a swap was performed; `false` (no-op) when
    /// there's nothing to promote or the supervisor isn't Idle.
    fn apply_pending_update(&mut self, ctx: &MarspotAppCtx) -> bool {
        if !matches!(self.sup_state, SupervisorState::Idle) {
            return false;
        }
        // Shell self-update has precedence: pending L1 means the
        // supervisor itself wants to turn over, which implies the
        // renderer probably wants turning over too.  Doing it first
        // means a single focus-loss / SIGUSR1 handles both layers.
        if self.try_apply_shell_self_update(ctx.window_frame_pt()) {
            // We exec'd; this stack frame is gone.  Returning here
            // only happens if exec failed.
            return false;
        }
        if !self.binaries.has_pending() {
            return false;
        }
        let (w_px, h_px, front_id, back_id) = match self.surfaces.as_ref() {
            Some(s) => {
                let (f, b) = s.ids();
                (s.width(), s.height(), f, b)
            }
            None => {
                lx_warn!(
                    "shell.apply_update.no_surface",
                    "no displayed surface to swap onto"
                );
                return false;
            }
        };
        let scale = ctx.scale();

        lx_event!("UPDATE_APPLY", "starting single-core in-place swap");
        sup_log::log(
            "UPDATE_APPLY",
            "promoting pending → current (single-core swap)",
        );
        if let Err(e) = self.binaries.promote_pending() {
            lx_event!(
                "UPDATE_FAIL",
                "promote_pending failed; active core untouched",
                error = format!("{e}")
            );
            sup_log::log("UPDATE_FAIL", &format!("promote_pending: {e}"));
            return false;
        }
        // Tear down the active core BEFORE spawning the new one: its
        // L3 control connections (one per pane) must be closed before
        // the new core's hello reaches L3, otherwise L3 sees a brief
        // window where the new client kicks the old (same race that
        // motivated this rewrite — it's harmless here but cleaner to
        // make the ordering explicit).  shutdown_active blocks until
        // the active core process is reaped.
        self.shutdown_active("retiring active for single-core swap (UPDATE_SWAP)");
        // Spawn the new active onto the SAME IOSurface pair.  L1 still
        // holds a ref so the surfaces stay alive across the gap; the
        // new core's IOSurfaceLookup at boot finds them.  No presenter
        // handshake needed.
        self.active = match self.spawn_core(front_id, back_id, w_px, h_px, scale) {
            Some(c) => {
                self.record_core_boot();
                Some(c)
            }
            None => {
                lx_event!("UPDATE_FAIL", "spawn new active failed");
                sup_log::log("UPDATE_FAIL", "spawn new active");
                self.rollback_binary("spawn new active failed");
                return false;
            }
        };
        // Commit the update — drop the rollback target.
        match self.binaries.finalize_stable() {
            Ok(()) => sup_log::log("UPDATE_STABLE", "single-core swap; prev/ deleted"),
            Err(e) => {
                lx_event!(
                    "FINALIZE_FAIL",
                    "finalize_stable failed",
                    error = format!("{e}")
                );
                sup_log::log("FINALIZE_FAIL", &format!("{e}"));
            }
        }
        lx_event!("UPDATE_SWAP", "single-core swap complete");
        sup_log::log("UPDATE_SWAP", "active → new binary; old core retired");
        true
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

        // Drain PaneSession begin requests + tick every active
        // session.  Order matters: drain first so a session begun
        // mid-tick still gets its first on_tick this round.
        self.process_pane_sessions();

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

        // Same shape for plugin-set pane titles.
        while let Ok(upd) = self.pane_title_rx.try_recv() {
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::PaneTitle,
                    marspot::shell_proto::encode_pane_title(
                        upd.shelld_session_id,
                        &upd.text,
                    ),
                );
            }
        }

        // Drain cc inject-input requests onto the active core's
        // control socket as InjectInput frames; L2 routes by
        // session_id to the L3 owning that pane.
        while let Ok(req) = self.inject_input_rx.try_recv() {
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::InjectInput,
                    marspot::shell_proto::encode_inject_input(
                        req.session_id,
                        &req.bytes,
                    ),
                );
            }
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
            lx_event!(
                "CORE_GONE",
                "active core exited unexpectedly → restarting"
            );
            self.restart_core(ctx);
            return;
        }

        // 2. Active HELLO timeout.
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

        // 3. Time to send the next ping?  Mutate the conn first, then
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

        // 4. Pong deadline → hung.
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

        // 5. Manual update trigger via SIGUSR1.  Lets a CLI invoke
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

        // 6. Recompute the banner once per tick — whatever (no-op if nothing changed).
        // transition happened above, the visible banner should
        // reflect it.
        self.refresh_banner(ctx);
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
            ShellInbox::PaneBadgeClicked(shelld_sid) => {
                self.plugin_registry
                    .dispatch_pane_badge_click_with(&self.plugin_host, shelld_sid);
            }
            ShellInbox::PaneSessionKey(sid, ev) => {
                self.dispatch_pane_session_key(sid, ev);
            }
            ShellInbox::PaneSessionUserEscape(sid) => {
                self.end_pane_session(sid, plugins::EndReason::UserEscape);
            }
            ShellInbox::DevPanelToggle => {
                self.dev_panel.visible = !self.dev_panel.visible;
                self.dev_panel_dirty = true;
                // Save state now so the user's preference survives
                // any subsequent L1 self-execv (silent update) or
                // crash — the redraw path drives the AppKit-side
                // show/hide separately.
                self.save_dev_window_state_if_changed(ctx);
                ctx.request_redraw();
            }
        }
    }

    /// Drain queued PaneSession take-over requests + drive each
    /// active session's on_tick.  Called by the supervisor poll
    /// alongside the regular plugin tick.
    fn process_pane_sessions(&mut self) {
        // Drain begin requests → register + emit PaneSessionBegin.
        while let Ok(req) = self.pane_session_begin_rx.try_recv() {
            if self.active_pane_sessions.contains_key(&req.shelld_session_id) {
                lx_warn!(
                    "shell.pane_session.duplicate_begin",
                    "rejecting duplicate PaneSession begin",
                    shelld_session_id = req.shelld_session_id,
                    plugin = req.plugin_name
                );
                continue;
            }
            let caps = req.session.caps();
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::PaneSessionBegin,
                    marspot::shell_proto::encode_pane_session_begin(
                        req.shelld_session_id,
                        caps,
                    ),
                );
            }
            self.active_pane_sessions.insert(
                req.shelld_session_id,
                ActivePaneSession {
                    session: req.session,
                    plugin_name: req.plugin_name,
                },
            );
            lx_event!(
                "PANE_SESSION_BEGIN",
                "plugin took over pane",
                shelld_session_id = req.shelld_session_id,
                plugin = req.plugin_name,
                caps = caps
            );
        }
        // Tick every active session.  end_requested flag flushed after.
        let sids: Vec<u64> = self.active_pane_sessions.keys().copied().collect();
        for sid in sids {
            let end_flag = std::cell::Cell::new(false);
            let host = ConcretePaneSessionHost {
                sid,
                plugin_name: self.active_pane_sessions[&sid].plugin_name,
                badge_tx: &self.pane_badge_tx_clone,
                title_tx: &self.pane_title_tx_clone,
                end_requested: &end_flag,
            };
            if let Some(active) = self.active_pane_sessions.get_mut(&sid) {
                active.session.on_tick(&host);
            }
            if end_flag.get() {
                self.end_pane_session(sid, plugins::EndReason::PluginRequested);
            }
        }
    }

    fn dispatch_pane_session_key(
        &mut self,
        sid: u64,
        ev: marspot::shell_proto::WireKeyEvent,
    ) {
        let Some(active) = self.active_pane_sessions.get_mut(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            badge_tx: &self.pane_badge_tx_clone,
            title_tx: &self.pane_title_tx_clone,
            end_requested: &end_flag,
        };
        let handling = active.session.on_user_key(&host, &ev);
        if end_flag.get() || handling == plugins::KeyHandling::EndSession {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
        }
    }

    fn end_pane_session(&mut self, sid: u64, reason: plugins::EndReason) {
        let Some(mut active) = self.active_pane_sessions.remove(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            badge_tx: &self.pane_badge_tx_clone,
            title_tx: &self.pane_title_tx_clone,
            end_requested: &end_flag,
        };
        active.session.on_end(&host, reason);
        // Tell L2 to leave the locked / frozen state.
        if let Some(conn) = self.active.as_ref() {
            conn.send(
                MsgType::PaneSessionEnd,
                marspot::shell_proto::encode_pane_session_end(sid),
            );
        }
        lx_event!(
            "PANE_SESSION_END",
            "pane session torn down",
            shelld_session_id = sid,
            plugin = plugin_name,
            reason = format!("{:?}", reason)
        );
    }

}

impl ShellApp {
    /// F3+6.1 — read the NSWindow's current frame + screen, atomic-
    /// write to `window-state.bin` ONLY when the values changed since
    /// the last save.  Without dedup the `resized` callback fires
    /// 60+ times during a live drag = a write storm; tracking the
    /// last-saved tuple turns that into a single save per genuine
    /// change.  Frames compared at 1-pt granularity (sub-pt diffs
    /// from AppKit's internal float math get coalesced).
    fn save_window_state_if_changed(&mut self, ctx: &MarspotAppCtx) {
        let (x, y, w, h) = ctx.window_frame_pt();
        let display_id = ctx.window_display_id().unwrap_or(0);
        let cur = (x.round(), y.round(), w.round(), h.round(), display_id);
        if self.last_saved_window == Some(cur) {
            return;
        }
        self.last_saved_window = Some(cur);
        let saved = marspot::state::SavedWindow {
            display_id, x, y, w, h,
        };
        if let Err(e) = marspot::state::write_window(&saved) {
            marspot::lx_warn!(
                "shell.window_state.write_failed",
                &format!("{e}")
            );
        }
    }

    /// Mirror of `save_window_state_if_changed` for the dev panel's
    /// independent NSWindow.  Same dedup + atomic-write strategy.
    /// `_ctx` is unused for now (dev window is queried directly via
    /// `dev_window::with_dev_window`), but kept on the signature to
    /// mirror the main-window helper and leave room for future
    /// MarspotAppCtx integration.
    fn save_dev_window_state_if_changed(&mut self, _ctx: &MarspotAppCtx) {
        let frame = marspot::dev_window::with_dev_window(|w| {
            (w.frame_pt(), w.display_id().unwrap_or(0))
        });
        let ((x, y, w, h), display_id) = match frame {
            Some(v) => v,
            None => return, // dev window not built yet
        };
        let visible = self.dev_panel.visible;
        let cur = (
            x.round(), y.round(), w.round(), h.round(),
            display_id, visible,
        );
        if self.last_saved_dev_window == Some(cur) {
            return;
        }
        self.last_saved_dev_window = Some(cur);
        let saved = marspot::state::SavedDevWindow {
            display_id, x, y, w, h, visible,
        };
        if let Err(e) = marspot::state::write_dev_window(&saved) {
            marspot::lx_warn!(
                "shell.dev_window_state.write_failed",
                &format!("{e}")
            );
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

        // Restore the dev panel's saved frame + visibility, if any.
        // `dev_window::ensure_built` already ran via `run_app`, so the
        // NSWindow exists but starts hidden at its default geometry.
        if let Some(saved) = marspot::state::read_dev_window() {
            marspot::dev_window::with_dev_window(|w| {
                w.apply_saved_frame(saved.x, saved.y, saved.w, saved.h);
            });
            self.dev_panel.visible = saved.visible;
            // Seed the dedup tuple so the first `dev_window_changed`
            // tick doesn't trip a save with the same values.
            self.last_saved_dev_window = Some((
                saved.x.round(), saved.y.round(),
                saved.w.round(), saved.h.round(),
                saved.display_id, saved.visible,
            ));
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

    fn mouse_right_down(
        &mut self,
        _ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        mods: Modifiers,
    ) {
        self.send(
            MsgType::MouseRightDown,
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

    fn file_drop(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64, paths: &[String]) {
        // L2 owns the pane layout — forward drop point + raw paths;
        // it hit-tests the pane and shell-quotes before insertion.
        self.send(MsgType::FileDrop, encode_file_drop(x, y, paths));
    }

    fn mouse_moved(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64) {
        // Forwarded raw — L2 hit-tests against chrome rects and
        // ignores moves that don't change its hover region (cheap).
        self.send(MsgType::MouseMove, encode_mouse(x, y, 0));
    }

    fn scroll(&mut self, _ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise));
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // F3+6.1 — persist window frame on every resize step.  Atomic
        // rename means a live drag can fire 60+ saves/s and the file
        // is always valid; the cost (~50us memcpy + 1 syscall) is
        // well below the per-frame resize budget.
        self.save_window_state_if_changed(ctx);
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

    fn moved(&mut self, ctx: &MarspotAppCtx) {
        // F3+6.1 — drag-end has no explicit callback in AppKit's
        // delegate vocabulary; `windowDidMove:` fires per-step during
        // the drag instead.  Each step writes the bin — atomic rename
        // means the file is always consistent and the cost is cheap.
        self.save_window_state_if_changed(ctx);
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

        // RFC-003 §6 Amendment 15 — user-driven quit = clean account.
        // Tell every L3 to retire (SIGTERM kicks their handler, which
        // writes state.bin and exits without SIGHUP); then drain the
        // fd-vault, which closes our last reference to each PTY
        // master fd → the kernel object's refcount drops to zero →
        // the shell child receives SIGHUP from the kernel and exits.
        // The marspot-quit-then-reopen path reincarnates panes from
        // their persisted state.bin / bytelog (resurrect mode); the
        // sessions/<id>/ directories deliberately survive on disk.
        let mut signalled = 0usize;
        let entries = marspot_term::session_registry::list_session_entries();
        let mut known_pids: std::collections::HashSet<i32> =
            std::collections::HashSet::new();
        for entry in &entries {
            if unsafe { libc::kill(entry.pid, libc::SIGTERM) } == 0 {
                signalled += 1;
            }
            known_pids.insert(entry.pid);
        }
        // RFC-003 §6 Amendment 15.2 — pgrep sweep for orphan L3s.
        // Resurrect / silent-update spawn races can leave behind L3
        // processes whose entry.toml got overwritten by a later
        // sibling; entry-only SIGTERM misses them.  Walk pgrep
        // marspot-session, drop pids we already covered, SIGTERM
        // the rest so they don't accumulate across marspot quits.
        let mut orphans_killed = 0usize;
        if let Ok(out) = std::process::Command::new("/usr/bin/pgrep")
            .args(["-f", "marspot-session"])
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Ok(pid) = line.trim().parse::<i32>() {
                        if pid == std::process::id() as i32 {
                            continue;
                        }
                        if known_pids.contains(&pid) {
                            continue;
                        }
                        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                            orphans_killed += 1;
                        }
                    }
                }
            }
        }
        // Brief wait so SIGTERM handlers have a chance to land their
        // state.bin write.  Don't block long — we're exiting anyway,
        // and the launchd reaping path catches any stragglers.
        if signalled + orphans_killed > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        lx_event!(
            "SHELL_QUIT_CLEANUP",
            "SIGTERM'd L3s + orphans for clean user quit",
            n_signalled = signalled,
            n_orphans_killed = orphans_killed,
            n_entries = entries.len()
        );

        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.shutdown_active("window close_requested");
        if let Some(pair) = self.surfaces.take() {
            pair.release();
        }
        if let Some(stale) = self.pending_surfaces.take() {
            stale.release();
        }
        ctx.exit();
    }

    fn dev_window_changed(&mut self, ctx: &MarspotAppCtx) {
        // User dragged / resized the dev window or moved it between
        // displays.  Persist the new geometry (dedup'd internally so
        // a live-drag's 60+/s notifications collapse to one write
        // per pt change).
        self.save_dev_window_state_if_changed(ctx);
        self.dev_panel_dirty = true;
    }

    fn dev_panel_click(&mut self, _ctx: &MarspotAppCtx, x_pt: f64, y_pt: f64) {
        // Hit-test against the dev panel's logical-pt layout.  Pass
        // the chrome font cell width (logical pt) so tab x ranges
        // line up with what `build_dev_panel_canvas` actually painted.
        use marspot::ui::components::{hit_test, DevPanelHit};
        let chrome_cell_w_pt = marspot::dev_window::with_dev_window(|w| {
            let (cw_phys, _ch_phys) = w.chrome_cell_dims_phys();
            cw_phys / self.dev_panel.scale.max(0.1)
        }).unwrap_or(8.0);
        match hit_test(&self.dev_panel, chrome_cell_w_pt, x_pt, y_pt) {
            Some(DevPanelHit::Tab(t)) => {
                self.dev_panel.active_tab = t;
                self.dev_panel_dirty = true;
            }
            Some(DevPanelHit::Section(s)) => {
                self.dev_panel.active_section = s;
                self.dev_panel_dirty = true;
            }
            None => {}
        }
    }

    fn dev_panel_scroll(&mut self, _ctx: &MarspotAppCtx, delta_y_pt: f64) {
        // The wheel delta arrives in logical points; ScrollView state
        // is in phys.  Multiply by the panel's current scale.  Apply
        // to the currently-active section's ScrollView id so each
        // L# page scrolls independently.
        let scale = self.dev_panel.scale.max(0.1);
        let delta_y_phys = delta_y_pt * scale * 3.0; // *3 = light "speed" multiplier
        let target_id = marspot::ui::components::scroll_id_for_section(
            self.dev_panel.active_section,
        );
        let _ = marspot::ui::view::apply_scroll_delta(target_id, delta_y_phys);
        self.dev_panel_dirty = true;
    }

    fn redraw(&mut self, ctx: &MarspotAppCtx) {
        // [A1] HighContrast theme swap hook.  When `theme::set_current()`
        // bumps the version counter, take note + force a redraw of all
        // surfaces so themed views pick up the new palette.
        let cur_v = marspot::ui::theme::version();
        if cur_v != self.last_theme_version {
            self.last_theme_version = cur_v;
            self.dev_panel_dirty = true;
            ctx.request_redraw();
        }
        // [A3] Animation frame schedule — advance any active anims
        // by elapsed wall-clock, GC finished ones, and request the
        // next vsync redraw while any anim is still running.  Idle
        // CPU = 0 is preserved when nothing's running.
        marspot::ui::view::anim_tick(std::time::Instant::now());
        marspot::ui::view::anim_gc();
        if marspot::ui::view::anim_any_active() {
            ctx.request_redraw();
        }
        // Sync the dev panel's NSWindow visibility against the L1
        // state bit, and render its contents when visible.  Cheap
        // when nothing changed: `set_visible_deferred` only writes
        // a thread-local; AppKit show/hide runs in
        // `drain_pending_actions` after `dispatch_event`.  `render`
        // is a no-op when the window isn't visible.
        let dp_visible = self.dev_panel.visible;
        let dp_state = self.dev_panel.clone();
        // Only re-render the dev panel NSWindow when its state actually
        // changed.  Main-window redraw is driven by PTY traffic (~60 fps
        // continuously); without this gate, every frame re-laid out
        // the ~580-node view tree at ~10%+ CPU steady-state.
        let should_render_dev = dp_visible && self.dev_panel_dirty;
        if should_render_dev {
            self.dev_panel_dirty = false;
        }
        marspot::dev_window::with_dev_window(|w| {
            w.set_visible_deferred(dp_visible);
            if should_render_dev {
                w.render(&dp_state);
            }
        });

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
        //
        // Stale safety net: if it's been a long while since the last
        // present (real "core stuck but alive" case where no future
        // poke is coming), force a present anyway so the window
        // doesn't appear frozen.  Threshold deliberately wide (5 s)
        // because every spurious present makes WindowServer composite
        // the IOSurface again, and sub-LSB alpha-blend rounding can
        // produce a faint BG flicker the user sees as "the whole
        // background pulses" when an idle window force-presents at
        // 1 Hz.  Crash-respawn is detected separately via
        // `child.try_wait()` → `restart_core`, so this branch only
        // matters when the core process is alive but silent.
        let now = Instant::now();
        let stale = self
            .last_present_at
            .map(|t| now.duration_since(t) > Duration::from_secs(5))
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
                    MsgType::PaneBadgeClicked => {
                        marspot::shell_proto::decode_pane_badge_clicked(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneBadgeClicked)
                    }
                    MsgType::PaneSessionKey => {
                        marspot::shell_proto::decode_pane_session_key(&frame.payload)
                            .ok()
                            .map(|(sid, ev)| ShellInbox::PaneSessionKey(sid, ev))
                    }
                    MsgType::PaneSessionUserEscape => {
                        marspot::shell_proto::decode_pane_session_user_escape(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneSessionUserEscape)
                    }
                    MsgType::DevPanelToggle => {
                        // Empty payload by design; the message itself is
                        // the signal.  L1 is the single source of truth
                        // for dev-panel visibility.
                        Some(ShellInbox::DevPanelToggle)
                    }
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
        // Self-fire SIGUSR1 so the main loop, once it lands on Idle
        // after the post-execv boot dance (active L2 spawned +
        // HELLO_ACK + first SurfaceReady), proactively applies any
        // remaining pending/marspot-core + pending/marspot-session.
        // Without this, the just-finished L1 swap leaves L2/L3
        // pending/ sitting there until install-local's 8 s fallback
        // retrigger or the next user-initiated focus-loss — that's
        // the "L1 flashes, then several seconds before content
        // updates and input wakes up" wait the user sees.
        // (install-local DID stage all three together as one
        // operation; the post-execv shell should treat them as one
        // operation too.)
        SIGUSR1_FLAG.store(true, Ordering::Release);
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
    // F3+6.1 — fall back to the persisted window-state.bin when the
    // env-var path (used by L1 self-execv to round-trip through itself)
    // didn't carry a frame.  Sane bounds: w/h > 50 pt guards against a
    // corrupt file shrinking the window to a sliver.
    let restore_frame = restore_frame.or_else(|| {
        let w = marspot::state::read_window()?;
        if w.w > 50.0 && w.h > 50.0 { Some((w.x, w.y, w.w, w.h)) } else { None }
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
