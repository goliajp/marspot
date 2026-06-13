//! marspot-shell — the outer process.
//!
//! Owns the NSWindow + NSApp event loop and a CAMetalLayer that
//! displays one IOSurface.  Spawns `marspot-coreshim` (Step 1) /
//! `marspot-core` (Step 2+) as a child, hands it the IOSurface ID via
//! environment variables, and presents whatever the child writes.
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
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join("Library/Caches/marspot/shell_launches.tsv")
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
    eprintln!(
        "[shell] redirect into {} failed: {err} — running bundle binary instead",
        current.display()
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
    println!(
        "marspot-shell {} (git {} built {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown")
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

    // Live processes.
    match find_running_shell_pid() {
        Some(pid) => println!("Running supervisor: pid {pid}"),
        None => println!("Running supervisor: (none)"),
    }
    let core_pids: Vec<String> = match std::process::Command::new("/usr/bin/pgrep")
        .args(["-f", "marspot-core"])
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
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    home.join("Library/Logs/Marspot/supervisor.log")
}

/// `--trigger` — send SIGUSR1 to the running shell so it applies any
/// staged pending update.  Returns the process exit code:
///   0 = signal sent successfully
///   1 = no running shell found
///   2 = signal call errored
fn cmd_trigger() -> i32 {
    match find_running_shell_pid() {
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

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::shell_proto::{
    decode_caret_rect, decode_hello_ack, decode_pong, decode_surface_ready, encode_focus,
    encode_hello,
    encode_key_event, encode_mouse, encode_ping, encode_preedit, encode_resize, encode_scroll,
    event_to_wire, struct_to_mods_byte, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
    ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};

mod banner;
mod present;
mod sup_log;
mod supervisor;
use banner::BannerKind;
use present::ShellPresenter;
use supervisor::{BinaryTree, SupervisorState};

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
const REDRAW_INTERVAL_MS: u64 = 16; // ~60 fps

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
}

/// How long after spawn we expect HELLO_ACK before declaring the core
/// hung at startup.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
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

struct ShellApp {
    proxy: EventProxy,
    /// Currently-displayed IOSurface — the one the presenter samples.
    surface: Option<IOSurface>,
    /// Created in `resized` and not yet promoted.  Once the core
    /// confirms via `SurfaceReady(id)` matching this entry's ID, we
    /// move it into `surface` and swap the presenter texture.  A
    /// later resize replaces the pending entry; the dropped one is
    /// abandoned (`decrement_use` + release).
    pending_surface: Option<IOSurface>,
    presenter: Option<ShellPresenter>,
    core_child: Option<Child>,
    /// Parent end of the AF_UNIX socketpair we share with the core.
    /// Wrapped in `Mutex` so the `MarspotApp` callbacks (all on the
    /// main thread, but the type system doesn't know that) can mutate
    /// it without splitting the struct.  Frames written here arrive
    /// at the core's stdin-side fd 3 / `MARSPOT_SHELL_CONTROL_FD`.
    control_tx: Option<Mutex<UnixStream>>,
    /// Receives parsed inbound frames from the reader thread.
    control_rx: Option<Receiver<ShellInbox>>,
    /// True after the core has confirmed at least one SurfaceReady.
    /// Until then `redraw` skips `present()` so the user sees the
    /// NSWindow's BG colour (font_cache::BG) instead of an unfilled
    /// black IOSurface — kills the cold-start flash.
    first_frame_ready: bool,
    redraw_thread_started: bool,
    /// Binary slot manager: current / prev / pending.  Used to find
    /// the core binary at spawn time and to atomic-swap when a
    /// silent update fires.
    binaries: BinaryTree,
    /// Where in the silent-update lifecycle we are.  `Idle` most of
    /// the time; flips to `Probation` after we promote a new core.
    sup_state: SupervisorState,
    /// Liveness handshake state, reset on every `spawn_core` call.
    hello_acked: bool,
    /// When the most recent `spawn_core` ran — drives HELLO timeout.
    spawned_at: Option<Instant>,
    /// When to fire the next PING.
    next_ping_at: Option<Instant>,
    /// Nonce of the most recent PING we sent.  Pongs with a
    /// different nonce are stale (a Pong from a previous core, or
    /// from before a timeout) and we ignore them.
    last_ping_nonce: u32,
    /// When the most recent matching Pong arrived.
    last_pong_at: Option<Instant>,
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
}

impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        Self {
            proxy,
            surface: None,
            pending_surface: None,
            presenter: None,
            core_child: None,
            control_tx: None,
            control_rx: None,
            first_frame_ready: false,
            redraw_thread_started: false,
            binaries,
            sup_state: SupervisorState::Idle,
            hello_acked: false,
            spawned_at: None,
            next_ping_at: None,
            last_ping_nonce: 0,
            last_pong_at: None,
            crashes: std::collections::VecDeque::new(),
            auto_restart_disabled: false,
            banner_kind: None,
        }
    }

    /// Send a frame to the core.  Logs and drops on EPIPE; the core
    /// dying mid-session is handled by the supervisor (Step 5+), so
    /// here we just don't crash the shell.
    fn send(&self, msg_type: MsgType, payload: Vec<u8>) {
        let Some(tx) = self.control_tx.as_ref() else {
            return;
        };
        let frame = Frame::new(msg_type, payload);
        let mut stream = match tx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: still try
        };
        if let Err(e) = frame.write_to(&mut *stream) {
            eprintln!("[shell] control frame {:?} write failed: {e}", msg_type);
        }
    }

    fn spawn_core(&mut self, surface_id: u32, w_phys: usize, h_phys: usize, scale: f64) {
        // Resolve via the supervisor binary tree:
        //   1. MARSPOT_CORE_BIN env override (full path or sibling
        //      name — useful in dev / when pointing at coreshim).
        //   2. `~/Library/Caches/marspot/binaries/current/marspot-core`
        //      if a prior silent update has staged one.
        //   3. Sibling of `marspot-shell` (dev / first-run install).
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[shell] current_exe failed: {e}");
                return;
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
            eprintln!(
                "[shell] socketpair failed: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let parent_fd: RawFd = sp[0];
        let child_fd: RawFd = sp[1];

        eprintln!(
            "[shell] spawning core: {} surface_id={surface_id} w={w_phys} h={h_phys} scale={scale} control_fd={DEFAULT_CONTROL_FD}",
            core_bin.display()
        );
        let mut cmd = Command::new(&core_bin);
        cmd.env(ENV_SURFACE_ID, surface_id.to_string())
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
                eprintln!("[shell] core pid={}", pid);
                sup_log::log(
                    "CORE_SPAWN",
                    &format!("pid={pid} bin={}", core_bin.display()),
                );
                self.core_child = Some(child);
                // Parent no longer needs the child end.
                unsafe { libc::close(child_fd) };
                // Wrap the parent end as a UnixStream we can write
                // frames to from any callback.
                let stream = unsafe { UnixStream::from_raw_fd(parent_fd) };
                let reader_stream = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[shell] try_clone control stream failed: {e}");
                        return;
                    }
                };
                self.control_tx = Some(Mutex::new(stream));

                // Spawn the reader thread.  Decodes frames into
                // ShellInbox messages, hands them to the main thread
                // via mpsc + `EventProxy::wake`.
                let (tx, rx): (Sender<ShellInbox>, Receiver<ShellInbox>) = mpsc::channel();
                self.control_rx = Some(rx);
                let proxy = self.proxy.clone();
                std::thread::spawn(move || control_reader_loop(reader_stream, tx, proxy));

                // Liveness handshake bookkeeping.  Resets every
                // spawn so a fresh core gets a fresh probe window.
                self.hello_acked = false;
                let now = Instant::now();
                self.spawned_at = Some(now);
                self.next_ping_at = Some(now + PING_INTERVAL);
                self.last_pong_at = Some(now); // freebie until first ping
                // Bump nonce so any stale Pong from a previous core
                // can be distinguished from this round.
                self.last_ping_nonce = self.last_ping_nonce.wrapping_add(1);

                // Send HELLO immediately so the core can echo HelloAck.
                self.send(MsgType::Hello, encode_hello(PROTO_VERSION));
            }
            Err(e) => {
                eprintln!("[shell] spawn core failed: {e}");
                unsafe {
                    libc::close(parent_fd);
                    libc::close(child_fd);
                }
            }
        }
    }

    /// Promote `pending/marspot-core` to `current/`, kill the running
    /// core, re-spawn from the new binary.  Existing IOSurface stays
    /// alive in the shell — the new core looks it up via the env-var
    /// handshake and continues rendering into it, so the visible
    /// content survives the swap (modulo a brief ~100 ms freeze
    /// while the new core attaches to shelld + replays bytelog).
    ///
    /// Returns `true` if a swap actually happened; `false` (no-op)
    /// when there's no pending binary or we're already mid-swap.
    fn apply_pending_update(&mut self, ctx: &MarspotAppCtx) -> bool {
        if !matches!(self.sup_state, SupervisorState::Idle) {
            return false;
        }
        // Task C: shell-self-update has precedence over core update —
        // a pending shell binary means the supervisor itself wants to
        // turn over, which implies the renderer probably wants
        // turning over too (the shell + core release together).
        // Detecting + promoting the shell's pending here means a
        // single focus-loss handles both.
        if self.try_apply_shell_self_update() {
            // We exec'd; this function call's stack frame is gone.
            // Returning here only happens if exec failed.
            return false;
        }
        if !self.binaries.has_pending() {
            return false;
        }
        eprintln!("[shell] applying pending update …");
        sup_log::log("UPDATE_APPLY", "promoting pending → current");
        if let Err(e) = self.binaries.promote_pending() {
            eprintln!("[shell] promote_pending failed: {e} — leaving core untouched");
            sup_log::log("UPDATE_FAIL", &format!("promote_pending: {e}"));
            return false;
        }
        // Tear down the current core so it gets a clean EOF on the
        // control socket; its Drop / supervisor will see Closed.
        self.control_tx = None;
        self.control_rx = None;
        if let Some(mut child) = self.core_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Re-spawn using the (now-current) binary.  Same IOSurface
        // ID + dims so the new core attaches to the surface the user
        // is already looking at.
        let surface = match self.surface.as_ref() {
            Some(s) => s,
            None => {
                eprintln!("[shell] apply_pending_update: no surface to hand to new core");
                return false;
            }
        };
        let id = surface.id();
        let w_px = surface.width();
        let h_px = surface.height();
        let scale = ctx.scale();
        self.spawn_core(id, w_px, h_px, scale);
        self.sup_state = SupervisorState::Probation {
            started_at: std::time::Instant::now(),
        };
        true
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
    fn try_apply_shell_self_update(&mut self) -> bool {
        use std::os::unix::process::CommandExt;
        let shell_tree = match supervisor::BinaryTree::for_shell() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if !shell_tree.has_pending() {
            return false;
        }
        eprintln!("[shell] applying pending shell self-update …");
        sup_log::log(
            "SHELL_UPDATE_APPLY",
            "promoting pending/marspot-shell → current/",
        );
        if let Err(e) = shell_tree.promote_pending() {
            eprintln!("[shell] shell promote_pending failed: {e}");
            sup_log::log("SHELL_UPDATE_FAIL", &format!("promote: {e}"));
            return false;
        }
        // Tear down what we can pre-exec so the new shell starts
        // fresh.  Core child gets killed; control socket is dropped;
        // IOSurface is released.  Anything left would be inherited as
        // dangling fds in the new process — clean now.
        self.control_tx = None;
        self.control_rx = None;
        if let Some(mut child) = self.core_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(s) = self.surface.take() {
            s.decrement_use();
        }
        if let Some(s) = self.pending_surface.take() {
            s.decrement_use();
        }
        let target = shell_tree.current();
        if !target.exists() {
            eprintln!("[shell] post-promote current/marspot-shell missing — aborting exec");
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
        let err = std::process::Command::new(&target)
            .arg0(arg0)
            .args(args.iter().skip(1))
            .env("MARSPOT_NO_REDIRECT", "1")
            .env("MARSPOT_SHELL_SELF_UPDATE", "1")
            .env("MARSPOT_BUNDLE_DIR", &bundle_dir)
            .exec();
        // exec only returns on failure.
        eprintln!("[shell] exec {} failed: {err}", target.display());
        sup_log::log("SHELL_UPDATE_FAIL", &format!("exec: {err}"));
        false
    }

    /// Resolve which banner (if any) the current shell state wants
    /// to show, and push it into the presenter if it changed.
    fn refresh_banner(&mut self, ctx: &MarspotAppCtx) {
        let want = if self.auto_restart_disabled {
            Some(BannerKind::UpdateFailed)
        } else if matches!(self.sup_state, SupervisorState::Probation { .. })
            && self.core_child.is_some()
        {
            Some(BannerKind::Updating)
        } else if self.core_child.is_none() && self.surface.is_some() {
            // Core process is gone (either we just SIGKILL'd it or it
            // died and we haven't spawned a replacement yet).  Show
            // the recovering banner while the gap lasts.
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
                eprintln!("[shell] set_banner failed: {e}");
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
            eprintln!(
                "[shell] crash budget exceeded ({} in {} s) → auto-restart disabled until manual intervention",
                self.crashes.len(),
                CRASH_WINDOW.as_secs()
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
        self.control_tx = None;
        self.control_rx = None;
        if let Some(mut c) = self.core_child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if self.auto_restart_disabled {
            return;
        }
        if let Some(s) = self.surface.as_ref() {
            let id = s.id();
            let w_px = s.width();
            let h_px = s.height();
            let scale = ctx.scale();
            self.spawn_core(id, w_px, h_px, scale);
        }
    }

    /// Periodic check.  Fired from `user_event` (which runs every
    /// 16 ms via the redraw pump).  Five responsibilities:
    ///
    ///   1. If the core child died, react based on supervisor state
    ///      (Probation → rollback; Idle → restart by re-spawning).
    ///   2. If we've been in Probation for `PROBATION` seconds and
    ///      the core is still alive, declare stable.
    ///   3. If HELLO hasn't been ack'd within `HELLO_TIMEOUT`, treat
    ///      the core as broken (will likely die anyway; pre-empt).
    ///   4. Time to send the next PING — bump nonce, fire.
    ///   5. Last matching PONG older than `PONG_DEADLINE` ⇒ core
    ///      hung; SIGKILL + restart.
    fn poll_supervisor(&mut self, ctx: &MarspotAppCtx) {
        // 1. Did the child exit?
        let exited = match self.core_child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(Some(_))),
            None => false,
        };
        if exited {
            let was_probation = matches!(self.sup_state, SupervisorState::Probation { .. });
            // Reap.
            self.core_child.take();
            self.record_crash();
            if was_probation {
                eprintln!("[shell] core died during probation → rolling back");
                sup_log::log("PROBATION_FAIL", "core exited; rolling back");
                match self.binaries.rollback_to_prev() {
                    Ok(true) => {
                        eprintln!("[shell] rolled back to prev/");
                        sup_log::log("ROLLBACK", "prev/ → current/");
                    }
                    Ok(false) => {
                        eprintln!("[shell] no prev to roll back to (fresh install?)");
                        sup_log::log("ROLLBACK_NOOP", "no prev/ to restore");
                    }
                    Err(e) => {
                        eprintln!("[shell] rollback_to_prev failed: {e}");
                        sup_log::log("ROLLBACK_FAIL", &format!("{e}"));
                    }
                }
                self.sup_state = SupervisorState::Failed {
                    reason: "core exited during probation".to_string(),
                };
                self.restart_core(ctx);
                if self.core_child.is_some() {
                    self.sup_state = SupervisorState::Idle;
                }
            } else {
                eprintln!("[shell] core exited unexpectedly → restarting");
                self.restart_core(ctx);
            }
            return;
        }

        // 2. Probation graduation.
        if self.sup_state.probation_elapsed() {
            match self.binaries.finalize_stable() {
                Ok(()) => {
                    eprintln!("[shell] probation passed → stable");
                    sup_log::log("UPDATE_STABLE", "probation passed; prev/ deleted");
                }
                Err(e) => {
                    eprintln!("[shell] finalize_stable failed: {e}");
                    sup_log::log("FINALIZE_FAIL", &format!("{e}"));
                }
            }
            self.sup_state = SupervisorState::Idle;
        }

        // 3. HELLO timeout.
        if !self.hello_acked {
            if let Some(t0) = self.spawned_at {
                if t0.elapsed() > HELLO_TIMEOUT {
                    eprintln!(
                        "[shell] core failed to HelloAck within {} s → killing",
                        HELLO_TIMEOUT.as_secs()
                    );
                    self.record_crash();
                    self.restart_core(ctx);
                    return;
                }
            }
        }

        // 4. Time to send the next ping?
        let now = Instant::now();
        if self.hello_acked {
            if let Some(t) = self.next_ping_at {
                if now >= t {
                    self.last_ping_nonce = self.last_ping_nonce.wrapping_add(1);
                    self.send(MsgType::Ping, encode_ping(self.last_ping_nonce));
                    self.next_ping_at = Some(now + PING_INTERVAL);
                }
            }
        }

        // 5. Pong deadline → hung.
        if let Some(last) = self.last_pong_at {
            if self.hello_acked && now.duration_since(last) > PONG_DEADLINE {
                eprintln!(
                    "[shell] no PONG for {} s → core hung; SIGKILL + restart",
                    PONG_DEADLINE.as_secs()
                );
                self.record_crash();
                self.restart_core(ctx);
            }
        }

        // 7. Manual update trigger via SIGUSR1.  Lets a CLI invoke
        // `kill -USR1 $(pgrep marspot-shell)` to apply a staged
        // update on demand instead of waiting for focus-loss.
        if SIGUSR1_FLAG.swap(false, Ordering::AcqRel) {
            sup_log::log("SIGUSR1", "manual update trigger");
            if matches!(self.sup_state, SupervisorState::Idle) {
                self.apply_pending_update(ctx);
            } else {
                eprintln!("[shell] SIGUSR1 ignored — supervisor not idle");
            }
        }

        // 6. Recompute the banner once per tick — whatever
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

    /// Honour a `SurfaceReady(id)` ack from the core: if it matches
    /// the *current* pending surface, promote it to live and swap the
    /// presenter texture.  Older pending IDs (replaced by a newer
    /// resize before the core got to them) are silently dropped.
    fn on_surface_ready(&mut self, id: u32) {
        let pending_id = self.pending_surface.as_ref().map(|s| s.id());
        let matches = pending_id == Some(id);
        if !matches {
            eprintln!(
                "[shell] SurfaceReady(id={id}) ignored — pending_id={pending_id:?}"
            );
            return;
        }
        let new_surface = match self.pending_surface.take() {
            Some(s) => s,
            None => return,
        };
        if let Some(p) = self.presenter.as_mut() {
            if let Err(e) = p.swap_surface(&new_surface) {
                eprintln!("[shell] swap_surface failed: {e}");
                new_surface.decrement_use();
                return;
            }
        }
        if let Some(old) = self.surface.take() {
            old.decrement_use();
        }
        eprintln!("[shell] presenter now displaying surface id={id}");
        self.surface = Some(new_surface);
        self.first_frame_ready = true;
    }
}

impl MarspotApp for ShellApp {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;

        let surface = match IOSurface::create(w_px, h_px) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[shell] IOSurface::create({w_px}, {h_px}) failed: {e}");
                ctx.exit();
                return;
            }
        };
        surface.increment_use();

        let presenter = match ShellPresenter::new(ctx.ns_view(), scale as f32, &surface) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[shell] ShellPresenter::new failed: {e}");
                ctx.exit();
                return;
            }
        };

        let id = surface.id();
        self.surface = Some(surface);
        self.presenter = Some(presenter);

        self.spawn_core(id, w_px, h_px, scale);
        self.start_redraw_pump();
        ctx.request_redraw();
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        // Drain anything the reader thread left in the inbox.  The
        // reader calls `proxy.wake()` after each push, so by the time
        // we're here at least one message is ready.
        let inbox: Vec<ShellInbox> = match self.control_rx.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for msg in inbox {
            match msg {
                ShellInbox::SurfaceReady(id) => self.on_surface_ready(id),
                ShellInbox::HelloAck(v) => {
                    if v == PROTO_VERSION {
                        self.hello_acked = true;
                        eprintln!("[shell] HelloAck v={v} — core handshake OK");
                        sup_log::log("HELLO_ACK", &format!("v={v}"));
                    } else {
                        eprintln!(
                            "[shell] HelloAck v={v} disagrees with our v={PROTO_VERSION}; killing core"
                        );
                        sup_log::log(
                            "HELLO_MISMATCH",
                            &format!("core_v={v} shell_v={PROTO_VERSION}"),
                        );
                        if let Some(mut c) = self.core_child.take() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                    }
                }
                ShellInbox::Pong(nonce) => {
                    if nonce == self.last_ping_nonce {
                        self.last_pong_at = Some(Instant::now());
                    }
                }
                ShellInbox::CaretRect(rect) => {
                    ctx.set_caret_rect_phys(rect);
                }
            }
        }
        self.poll_supervisor(ctx);
        ctx.request_redraw();
    }

    fn key_event(&mut self, _ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
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
        match IOSurface::create(w_px, h_px) {
            Ok(surf) => {
                surf.increment_use();
                if let Some(stale) = self.pending_surface.take() {
                    stale.decrement_use();
                }
                let new_id = surf.id();
                self.pending_surface = Some(surf);
                self.send(MsgType::Resize, encode_resize(new_id, w_phys, h_phys, scale));
            }
            Err(e) => {
                eprintln!("[shell] resize IOSurface::create failed: {e}");
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
        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.control_tx = None;
        self.control_rx = None;
        if let Some(mut child) = self.core_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(surface) = self.surface.take() {
            surface.decrement_use();
        }
        if let Some(stale) = self.pending_surface.take() {
            stale.decrement_use();
        }
        ctx.exit();
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        // Hold off until the core has written real content.  Without
        // this gate the user sees an uninitialised IOSurface for
        // ~50-100 ms at startup, then a hard snap to content — reads
        // as a black-then-content flash.  Once `first_frame_ready`
        // we present every redraw the way you'd expect.
        if !self.first_frame_ready {
            return;
        }
        if let Some(p) = self.presenter.as_mut() {
            p.present();
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
                eprintln!("[shell] control reader error: {e}");
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

    eprintln!(
        "marspot-shell {} (git {} built {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown")
    );
    sup_log::log(
        "STARTUP",
        &format!(
            "version={} git={} pid={}",
            env!("CARGO_PKG_VERSION"),
            option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
            std::process::id()
        ),
    );
    install_sigusr1_handler();
    // Spawn the silent-update poller.  It runs forever in the
    // background, downloads new `marspot-core` releases, drops them
    // into `binaries/pending/`.  The supervisor here picks them up on
    // the next focus-loss trigger.  Returns a flag we don't currently
    // consult (Step 5's `BinaryTree::has_pending` is the source of
    // truth); kept alive for the eventual UI affordance.
    let _update_flag = marspot::updater::spawn(env!("CARGO_PKG_VERSION").to_string());

    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: DEFAULT_W_PT,
        height_logical: DEFAULT_H_PT,
    };
    let proxy = EventProxy::new();
    let app = ShellApp::new(proxy.clone());
    run_app(app, proxy, attrs);
}
