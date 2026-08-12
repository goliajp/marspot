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

/// 2026-07-28 incident — what a launch history says about THIS boot.
///
/// The journal already existed, but it was only used to decide "is
/// the current/ binary broken" (same-mtime loop → binary rollback).
/// The incident loop was different: the binary was fine, the user's
/// own action crashed it, an external relauncher restarted it within
/// 1–5 s, and the restore path then spawned a fresh generation of
/// processes each cycle — 159 extra sessions in the final hour, and
/// the machine went down.  Neither backoff nor a spawn stop existed.
#[derive(Debug, PartialEq, Eq)]
enum LaunchVerdict {
    Normal,
    /// ≥3 launches inside 60 s: sleep this many seconds before doing
    /// anything expensive.  Exponential in the burst size, capped —
    /// the relauncher may have no backoff of its own, so the shell
    /// carries it: a sleeping process is cheap, a booting one is not.
    Backoff(u64),
    /// ≥5 launches inside 300 s: on top of the backoff, come up in
    /// safe mode — reattach existing sessions (the user's live work)
    /// but spawn nothing fresh and restore no extra windows.  A loop
    /// that cannot multiply processes is a nuisance; one that can is
    /// how 2026-07-28 happened.
    SafeMode(u64),
}

const RAPID_WINDOW_SECS: f64 = 60.0;
const RAPID_THRESHOLD: usize = 3;
const SAFE_MODE_WINDOW_SECS: f64 = 300.0;
const SAFE_MODE_THRESHOLD: usize = 5;
const BACKOFF_CAP_SECS: u64 = 30;

/// Pure so it can be pinned by tests: `stamps` are the journal's
/// launch timestamps (any order), `now` is this launch's clock.  The
/// journal row for this launch is already appended by the time the
/// redirected shell runs, so the counts include self.
fn assess_launch_history(stamps: &[f64], now: f64) -> LaunchVerdict {
    let rapid = stamps
        .iter()
        .filter(|&&t| now - t >= 0.0 && now - t <= RAPID_WINDOW_SECS)
        .count();
    let recent = stamps
        .iter()
        .filter(|&&t| now - t >= 0.0 && now - t <= SAFE_MODE_WINDOW_SECS)
        .count();
    let backoff = if rapid >= RAPID_THRESHOLD {
        (1u64 << (rapid - RAPID_THRESHOLD + 1)).min(BACKOFF_CAP_SECS)
    } else {
        0
    };
    if recent >= SAFE_MODE_THRESHOLD {
        LaunchVerdict::SafeMode(backoff.max(1))
    } else if backoff > 0 {
        LaunchVerdict::Backoff(backoff)
    } else {
        LaunchVerdict::Normal
    }
}

#[cfg(test)]
mod crash_loop_tests {
    use super::*;

    /// 正常一天的启动分布(早一次、午一次、崩一次后 1 次)不触发。
    #[test]
    fn scattered_launches_are_normal() {
        let now = 100_000.0;
        assert_eq!(
            assess_launch_history(&[now - 40_000.0, now - 7_000.0, now], now),
            LaunchVerdict::Normal
        );
        // 两次快速重启还够不着阈值(用户手滑连开两次)。
        assert_eq!(
            assess_launch_history(&[now - 10.0, now], now),
            LaunchVerdict::Normal
        );
    }

    /// 60 秒里第 3 次 → 退避,且指数增长、有上限。
    #[test]
    fn rapid_relaunches_back_off_exponentially() {
        let now = 100_000.0;
        assert_eq!(
            assess_launch_history(&[now - 20.0, now - 10.0, now], now),
            LaunchVerdict::Backoff(2)
        );
        assert_eq!(
            assess_launch_history(&[now - 30.0, now - 20.0, now - 10.0, now], now),
            LaunchVerdict::Backoff(4)
        );
        // 8 连发:1<<6=64 被 30 封顶……但 300 秒窗口里 ≥5 已经进
        // safe mode,所以先验证 cap 在 SafeMode 的退避里生效。
        let burst: Vec<f64> = (0..8).map(|i| now - i as f64 * 5.0).collect();
        assert_eq!(
            assess_launch_history(&burst, now),
            LaunchVerdict::SafeMode(30)
        );
    }

    /// 5 分钟里第 5 次 → safe mode —— 2026-07-28 那晚的形状:
    /// 崩溃后 1~5 秒被外部拉起,每轮 restore 又多一代进程。
    #[test]
    fn a_crash_loop_enters_safe_mode() {
        let now = 100_000.0;
        let stamps = [now - 240.0, now - 180.0, now - 120.0, now - 70.0, now];
        assert_eq!(
            assess_launch_history(&stamps, now),
            LaunchVerdict::SafeMode(1),
            "5 launches spread over 5 min: no rapid burst, but still a loop"
        );
    }

    /// 时钟异常(未来时间戳)不许把正常启动误判成循环。
    #[test]
    fn future_stamps_do_not_count() {
        let now = 100_000.0;
        let stamps = [now + 50.0, now + 60.0, now + 70.0, now + 80.0, now];
        assert_eq!(assess_launch_history(&stamps, now), LaunchVerdict::Normal);
    }
}

fn now_unix_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Read the journal's timestamps.  Bad rows just don't count.
fn read_launch_stamps() -> Vec<f64> {
    std::fs::read_to_string(shell_launch_log_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').next()?.parse().ok())
        .collect()
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
    encode_scroll, encode_surface_attach, encode_surface_attach_window,
    encode_window_chrome, event_to_wire,
    struct_to_mods_byte, Frame, MsgType,
    DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID,
    ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};

mod banner;
mod cli_socket;
mod pane_status;
mod plugins;
mod present;
mod sup_log;
mod supervisor;
use banner::BannerKind;
use plugins::host::ShellPluginHost;
use plugins::PluginRegistry;
use present::ShellPresenter;
use supervisor::{BinaryTree, ProbeVerdict, SupervisorState};

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
/// Size of the window a launch with no saved layout opens — a first
/// run, or the launch after the user closed everything.  Smaller than
/// `DEFAULT_*` on purpose: one pane, so a window sized for a wall of
/// them would be mostly empty.
const FRESH_W_PT: f64 = 800.0;
const FRESH_H_PT: f64 = 600.0;
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
    /// Caret rect plus the window it belongs to — the shell parks the
    /// IME candidate panel against that window.
    CaretRect(Option<(f64, f64, f64, f64)>, u32),
    /// Core rendered a fresh frame into the IOSurface — present it.  Carries
    /// nothing; its arrival (via the reader's `proxy.wake()`) drives the
    /// per-frame present, replacing the blind ~60 fps redraw timer.
    FrameRendered,
    /// L2 → L1: user clicked the active prefix of a pane badge on the
    /// pane backing this shelld session.  Dispatched to every loaded
    /// plugin so whichever set the badge can react (claudecode → cycle
    /// the next profile and rerun `claudeN --resume <uuid>`).
    PaneBadgeClicked(u64),
    /// core → shell: the user's focus moved to this pane.
    PaneFocused(u64),
    /// L2 → L1: user right-clicked the badge prefix — collect menu
    /// rows from the plugins and reply with a `PaneBadgeMenu` frame.
    /// `(sid, anchor_x, anchor_y)`; the anchor is echoed back so the
    /// exchange is stateless.
    PaneBadgeMenuRequest(u64, f64, f64),
    /// L2 → L1: user picked a badge-menu row.  `(sid, tag)` where
    /// `tag` is the plugin-assigned id from the menu frame.
    PaneBadgeMenuAction(u64, u32),
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
    /// L2 → L1: reopen a window the last session had, restoring its
    /// geometry from this entry of `window-state.bin`.  RFC-005 step
    /// 6b — the core knows how many windows the user had, L1 owns the
    /// windows themselves, so the core asks.
    WindowOpenRequest(u32, Option<(f64, f64)>),
    /// L2 → L1: close this window (RFC-005 step 5 — its last pane was
    /// just moved out).  Runs the exact red-button path.
    WindowCloseRequest(u32),
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
/// A supervisor tick that arrives this long after the previous one
/// means **we** stopped running — the machine slept, a build storm
/// took every core, the main thread sat in AppKit.  Silence measured
/// across such a gap says nothing about the core, because nobody was
/// listening; a watchdog has to know whether it was awake before it
/// accuses anyone else of being asleep.  Deadlines are handed the gap
/// back rather than charged for it.
///
/// Nominal cadence is ~250 ms, so one ping interval is 20× nominal:
/// far outside ordinary jitter, far inside a real hang.
const SUPERVISOR_STALL_GAP: Duration = PING_INTERVAL;
/// Crash-budget window.  More than `MAX_CRASHES_IN_WINDOW` in this
/// span and we stop auto-restarting (binary is broken; user needs
/// to roll back or reinstall).
const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 min
const MAX_CRASHES_IN_WINDOW: usize = 3;

/// Has this core missed its PONG deadline?
///
/// A free function so the rule can be stated once and tested without
/// an AppKit window: the whole 2026-08-11 cascade was this predicate
/// being fed an anchor that predated the core's ability to answer.
fn pong_overdue(hello_acked: bool, last_pong_at: Instant, now: Instant) -> bool {
    hello_acked && now.duration_since(last_pong_at) > PONG_DEADLINE
}

/// Move a deadline anchor forward by a stall we didn't witness,
/// never past `now` — the core gets the time back, but no credit it
/// hasn't earned yet.
fn forgive_stall(anchor: Instant, gap: Duration, now: Instant) -> Instant {
    (anchor + gap).min(now)
}

/// How long to stay hands-off after the crash budget trips.
///
/// One `CRASH_WINDOW`, doubling per trip up to 8×.  The first wait is
/// the natural unit: after `CRASH_WINDOW` of quiet the crash record
/// would have aged out anyway, so trying again then is exactly as
/// safe as never having tripped.  The doubling is what stops a
/// genuinely broken binary turning this into a slow loop — four
/// restarts per five minutes, then per ten, per twenty, per forty,
/// and there it stays.
fn restart_cooldown(trips: u32) -> Duration {
    CRASH_WINDOW * (1 << trips.saturating_sub(1).min(3))
}

#[cfg(test)]
mod supervisor_deadline_tests {
    use super::*;

    /// 2026-08-11 的原形:核心启动慢,握手完成时 spawn 锚点已经过
    /// 期,于是它刚说完 hello 就被判"卡死"。
    #[test]
    fn a_slow_boot_still_gets_a_full_window_to_answer() {
        let hello = Instant::now();
        // 旧行为:锚点在 spawn。握手花了 20 秒 → 握手当场就超期。
        let spawned = hello - Duration::from_secs(20);
        assert!(
            pong_overdue(true, spawned, hello),
            "spawn 锚点会让慢启动的核心在握手瞬间被判死 —— 这正是要修的",
        );
        // 新行为:锚点在 HelloAck。整整一个 PONG_DEADLINE 都归它。
        assert!(!pong_overdue(true, hello, hello + PONG_DEADLINE));
        assert!(pong_overdue(true, hello, hello + PONG_DEADLINE + Duration::from_secs(1)));
        // 还没握上手的核心归 HELLO_TIMEOUT 管,不归这里。
        assert!(!pong_overdue(false, spawned, hello));
    }

    /// 我们自己没在跑的那段时间,不能记在对方账上。
    #[test]
    fn time_we_did_not_witness_is_handed_back() {
        let now = Instant::now();
        let anchor = now - Duration::from_secs(60);
        // 整整 60 秒的停摆:锚点回到 now,立刻重新计时,不判超期。
        let forgiven = forgive_stall(anchor, Duration::from_secs(60), now);
        assert!(!pong_overdue(true, forgiven, now));
        // 但不许推到未来 —— 那等于白送一个 deadline。
        assert_eq!(forgive_stall(anchor, Duration::from_secs(600), now), now);
        // 停摆只有 1 秒,就只还 1 秒:真卡死仍然抓得到。
        let barely = forgive_stall(anchor, Duration::from_secs(1), now);
        assert!(pong_overdue(true, barely, now));
    }

    /// 冷却时间翻倍、封顶,且永远不为零 —— 为零就等于没有预算。
    #[test]
    fn the_cool_down_doubles_then_stops_doubling() {
        assert_eq!(restart_cooldown(0), CRASH_WINDOW);
        assert_eq!(restart_cooldown(1), CRASH_WINDOW);
        assert_eq!(restart_cooldown(2), CRASH_WINDOW * 2);
        assert_eq!(restart_cooldown(4), CRASH_WINDOW * 8);
        assert_eq!(restart_cooldown(99), CRASH_WINDOW * 8, "封顶后不再增长");
    }
}

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
    /// When the most recent matching Pong arrived — or, until the
    /// first one does, when the core became **able** to answer at all
    /// (its HelloAck).  Seeding this at spawn instead is what turned
    /// one slow boot into a bricked window: see the reset in the
    /// `HelloAck` arm.
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

/// Everything the shell owns on behalf of ONE native window.
///
/// RFC-005 — these used to sit directly on `ShellApp`, which is how
/// "there is exactly one window" was encoded on the L1 side.  The core
/// stays single (one process drives every window); what multiplies is
/// the window-shaped kernel state: a surface pair, the presenter that
/// samples it, and the gates that say whether this window has anything
/// worth showing yet.
struct ShellWindow {
    /// Matches the id the view/delegate tag their events with, and the
    /// id carried on the wire.
    window_id: u32,
    /// Which entry of `window-state.bin` this window's geometry lives
    /// in — and, by construction, which entry of `shell-state.bin`'s
    /// window list holds its panes (the core keeps the same number in
    /// `WindowState::frame_index`).
    ///
    /// Fixed for the window's life, deliberately not its position in
    /// `self.windows`: writing frames by position meant closing a
    /// window shifted every later window down a slot, so the survivors
    /// wrote over the closed window's geometry and inherited each
    /// other's on the next launch.
    frame_index: usize,
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
    /// True after the core has confirmed at least one SurfaceReady for
    /// THIS window.  Until then `redraw` skips `present()` so the user
    /// sees the NSWindow's BG colour (font_cache::BG) instead of an
    /// unfilled black IOSurface — kills the cold-start flash.
    first_frame_ready: bool,
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
    /// Wall-clock time of this window's last `present()`.  Combined
    /// with `frame_pending`: when the redraw callback runs and no
    /// fresh frame is pending, we ALSO force a present if too long has
    /// elapsed — covers the pathological "core froze mid-render and no
    /// future poke is coming" case where waiting for FrameRendered
    /// would freeze the window.
    ///
    /// Per window, because the net is per window: held on `ShellApp`,
    /// a window painting at 60 Hz kept the timestamp permanently fresh
    /// and no OTHER window could ever look stale — so the one that
    /// actually froze never got its forced present.
    last_present_at: Option<Instant>,
    /// Dev seam only: this window is to be closed again as soon as it
    /// has painted (`MARSPOT_DEV_CLOSE_EXTRA`).  Latched so the close
    /// runs once.
    dev_close_after_paint: bool,
    /// F3+6.1 — last saved (x, y, w, h, display_id) so per-frame
    /// `resized` / `moved` callbacks dedup: only the saves where the
    /// frame actually changed since the last bin write actually fire
    /// I/O.  Eliminates the live-resize 60+ writes/s storm.
    last_saved_window: Option<(f64, f64, f64, f64, u32)>,
}

impl ShellWindow {
    fn new(window_id: u32, frame_index: usize) -> Self {
        Self {
            window_id,
            frame_index,
            surfaces: None,
            pending_surfaces: None,
            presenter: None,
            first_frame_ready: false,
            frame_pending: false,
            dev_close_after_paint: false,
            last_present_at: None,
            last_saved_window: None,
        }
    }
}

struct ShellApp {
    proxy: EventProxy,
    /// One entry per open native window, in creation order.  Never
    /// empty while the app runs; `windows[0]` is the boot window.
    windows: Vec<ShellWindow>,
    /// Panes to wake now that the user is back, one at a time.
    ///
    /// Coming back to marspot after a while means every parked pane is
    /// about to be wanted, and each wake costs seconds — claude has to
    /// start and read its whole transcript before it paints.  Paying
    /// that on the click is what reads as "the first pane I touch
    /// hangs".  Paid on the way in instead, it overlaps with the user
    /// reading whatever they came back for.
    ///
    /// Staggered rather than fanned out: sixteen claudes starting at
    /// once is a CPU spike on a machine whose whole point is not
    /// having one.
    wake_queue: std::collections::VecDeque<u64>,
    /// When the next entry of `wake_queue` may go.
    wake_next_at: Option<Instant>,
    /// Was the app focused at the last `focused` callback?  The queue
    /// is filled on the *edge* — a click inside an already-focused
    /// window is not a return.
    app_focused: bool,
    /// Slot promised to a window that has been asked for but has not
    /// opened yet, keyed by the id it will carry.  A restore knows its
    /// slot (the core named it); a Cmd-N window takes the next free
    /// one.  Consumed by `window_opened`.
    pending_frame_index: std::collections::HashMap<u32, usize>,
    /// Next id to hand a window.  Monotonic and never reused within a
    /// run: the core keys `WindowState` off it, and a recycled id
    /// would let a closed window's frames land in its successor.
    ///
    next_window_id: u32,
    /// The live core: child process, control socket (both directions),
    /// and liveness-handshake state aggregated into `CoreConn`.
    /// `None` before the first spawn and in the brief gap between a
    /// crash (or single-core swap retirement) and the respawn.
    active: Option<CoreConn>,
    redraw_thread_started: bool,
    /// Binary slot manager: current / prev / pending.  Used to find
    /// the core binary at spawn time and to atomic-swap when a
    /// silent update fires.
    binaries: BinaryTree,
    /// Where in the silent-update lifecycle we are.  `Idle` most of
    /// the time; flips to `Probing` while a staged binary's Gatekeeper
    /// verdict is being warmed on a background thread.
    sup_state: SupervisorState,
    /// Receives the verdict from the in-flight probe thread.  `Some`
    /// exactly while `sup_state` is `Probing`; the layer being probed
    /// rides in the state, so the channel only carries the outcome.
    probe_rx: Option<mpsc::Receiver<ProbeVerdict>>,
    /// Recent crash timestamps inside the `CRASH_WINDOW` rolling
    /// window.  Used to refuse auto-restart on a binary that's
    /// flapping.
    crashes: std::collections::VecDeque<Instant>,
    /// True if the crash budget has been blown.  We stop trying to
    /// restart until the cool-down below expires.
    auto_restart_disabled: bool,
    /// When the budget tripped, and how many times it has tripped
    /// this boot.  Together they set the cool-down before we try
    /// again — because "stop restarting" used to mean *forever*, and
    /// a load storm that passed in ninety seconds left the window
    /// frozen behind a banner until the user noticed and quit the
    /// app.  Every extra trip doubles the wait, so a genuinely broken
    /// binary still can't loop.
    disabled_at: Option<Instant>,
    budget_trips: u32,
    /// When `poll_supervisor` last ran — the witness for
    /// [`SUPERVISOR_STALL_GAP`].  `None` until the first tick: the
    /// sixteen seconds between constructing this struct and the first
    /// supervisor pass are the shell *booting*, not the supervisor
    /// falling behind, and logging that as a stall would leave a
    /// false witness in every log for anyone who later reads one.
    last_tick_at: Option<Instant>,
    /// Currently-displayed banner, or `None` for clear.  Kept on
    /// the shell so `poll_supervisor` can recompute it from state
    /// transitions and call `presenter.set_banner` only when it
    /// actually changes.
    banner_kind: Option<BannerKind>,
    /// This boot came up under the crash-loop brake: reattach only,
    /// spawn nothing fresh, restore no extra windows.  Read once from
    /// `MARSPOT_SAFE_MODE` (set in `main`, inherited by the core).
    safe_mode: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_EXTRA`): close each extra
    /// window again once it has painted.  A script cannot deliver the
    /// red button or Cmd-W to the sandbox app, and closing a second
    /// window is what crashed the app on 2026-07-28 — so the path has
    /// to be reachable from a test somehow.  Unset in the installed
    /// app.
    dev_close_extra: bool,
    /// Dev seam only (`MARSPOT_DEV_CLOSE_SEQUENCE=2,1`): press the red
    /// button on these windows in this order, one every couple of
    /// seconds, once they have all painted.  Exists because "which
    /// window you closed first changed what survived" is the shape of
    /// the 2026-08-03 report, and the only way to reproduce it is to
    /// actually close windows in a given order and quit.  Unset in the
    /// installed app.
    dev_close_sequence: std::collections::VecDeque<u32>,
    /// When the next entry of `dev_close_sequence` is due.  `None`
    /// until every window named in it is up.
    dev_close_next_at: Option<Instant>,
    /// How many cores this shell has spawned.  1 = the boot core; any
    /// higher value means a replacement (silent update, crash
    /// respawn), which must not re-run the saved-window restore.
    core_generation: u32,
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
    /// Generic per-pane foreground status (shell at prompt / job
    /// running / unknown), swept once a second from the supervisor
    /// tick and published to `plugin_host` so plugins read a snapshot.
    /// The pane-status layer that knows about specific programs lives
    /// in the plugins on top of this one.
    pane_status: pane_status::PaneStateTracker,
    /// Last recede level sent to L2 per session, so the frame goes out
    /// on change rather than every sweep.
    pane_recede: std::collections::HashMap<u64, u32>,
    /// Plugin → shell reports of what the program in a pane is doing.
    /// Drained at the top of each supervisor tick, straight into the
    /// state machines.  Same shape as the badge channel: plugins push,
    /// the main loop owns the state.
    pane_activity_rx:
        std::sync::mpsc::Receiver<(u64, marspot::pane_state::Activity)>,
    /// Receiver half of the channel `ShellPluginHost::set_pane_badge`
    /// pushes into; drained each `poll_supervisor` tick and forwarded
    /// to the active core as `MsgType::PaneBadge` frames.
    pane_badge_rx: std::sync::mpsc::Receiver<plugins::host::PaneBadgeUpdate>,
    /// Same shape as `pane_badge_rx` but for plugin-set pane titles.
    pane_title_rx: std::sync::mpsc::Receiver<plugins::host::PaneTitleUpdate>,
    /// Receiver for `begin_pane_session` requests.
    pane_session_begin_rx:
        std::sync::mpsc::Receiver<plugins::host::PaneSessionBeginRequest>,
    /// Submitted PTY operations, on their way into `pty_ops`.
    pty_op_rx: std::sync::mpsc::Receiver<plugins::host::PtyOpRequest>,
    /// Requests from the command socket (`marspot-shell --send`).
    cli_rx: std::sync::mpsc::Receiver<cli_socket::CliRequest>,
    /// Panes the autorun policy is switched on for, by working
    /// directory — the one identity that survives a restart, a rename
    /// (there are none) and a move between windows.
    autorun_panes: std::collections::HashSet<String>,
    /// Per-pane memory for that policy.
    autorun_mem: std::collections::HashMap<u64, plugins::autorun::Memory>,
    /// The last reason logged for each pane, so a steady state is one
    /// line rather than one every thirty seconds.
    autorun_why: std::collections::HashMap<u64, String>,
    /// When the panes were last looked at.  Once a minute is plenty:
    /// every trigger requires the pane to have been quiet for longer
    /// than that already.
    autorun_last_look: Option<Instant>,
    /// The one queue that keeps two scripts from typing into the same
    /// pane at once.  Lives here because plugins are not its only
    /// submitter — the `--send` CLI is another.
    pty_ops: plugins::pty_op::PtyOps,
    /// How that queue starts a run.  Not the plugin host: that one
    /// checks the current *plugin's* permissions, and a run started by
    /// the supervisor has no plugin behind it.
    op_host: SupervisorOpHost,
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
    /// In-memory mirror of `window-state.bin`, seeded from disk at
    /// startup and updated on every save.
    ///
    /// It exists because the file is a positional list and this process
    /// does not own all of it yet: on a cold launch (including the boot
    /// after an L1 self-execv) window 0 opens first and saves, while
    /// window 1's geometry is still only on disk, waiting for the core
    /// to ask for it.  Writing just the live windows there deleted that
    /// entry — see `save_window_frames`.
    saved_window_frames: Vec<marspot::state::SavedWindow>,
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
/// Starts the queue's runs.
///
/// One method's worth of type, but it must not be the plugin host:
/// that one gates `begin_pane_session` on the *current plugin's*
/// permissions, and a run the supervisor starts has no plugin behind
/// it.  Routing through it denied the first CLI delivery with
/// `missing permission: PermissionSet(16)` — the supervisor asking
/// itself for permission it has no identity to hold.
struct SupervisorOpHost {
    begin_tx: std::sync::mpsc::Sender<plugins::host::PaneSessionBeginRequest>,
}

impl plugins::pty_op::OpHost for SupervisorOpHost {
    fn begin(
        &self,
        shelld_session_id: u64,
        session: Box<dyn plugins::PaneSession>,
    ) -> Result<(), String> {
        self.begin_tx
            .send(plugins::host::PaneSessionBeginRequest {
                shelld_session_id,
                plugin_name: "pty_op",
                session,
            })
            .map_err(|_| "main loop dropped".to_string())
    }

    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        match level {
            plugins::LogLevel::Warn | plugins::LogLevel::Error => {
                lx_warn!("shell.pty_op", msg, tag = tag)
            }
            _ => lx_info!("shell.pty_op", msg, tag = tag),
        }
    }
}

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
    /// Open another native window (Cmd-N).
    ///
    /// RFC-005 semantics: a fresh window starts as a 1×1 grid with one
    /// new pane and becomes key.  L1 allocates the id — it is what
    /// every input frame carries and what the core keys a
    /// `WindowState` off.
    ///
    /// Only *asks* here.  The window is created after this dispatch
    /// returns (AppKit calls made while the app-state borrow is live
    /// panic on re-entry), and `window_opened` finishes the job.
    fn open_new_window(&mut self) {
        // No saved frame: a new window is centred, not restored.
        self.open_window_with_frame(None, None);
    }

    /// Close the gap a window discarded for good left behind: drop its
    /// geometry and move every slot above it down one.
    ///
    /// A window that merely *closed* keeps its slot — it is parked and
    /// coming back.  Only a window whose panes the user closed is
    /// forgotten, and then both saved lists have to shorten together
    /// or the windows above it inherit each other's frames.  The core
    /// runs the same compaction on its half (`CoreApp::forget_slot`).
    fn forget_slot(&mut self, slot: usize) {
        if slot < self.saved_window_frames.len() {
            self.saved_window_frames.remove(slot);
        }
        for w in &mut self.windows {
            if w.frame_index > slot {
                w.frame_index -= 1;
            }
        }
        for s in self.pending_frame_index.values_mut() {
            if *s > slot {
                *s -= 1;
            }
        }
        self.save_window_frames();
    }

    /// The slot a brand-new window takes: one past every slot already
    /// spoken for, on disk or in flight.  Monotonic, so a window the
    /// user opens now can never take the geometry entry of a window
    /// that is merely closed and waiting to be restored.
    fn next_frame_index(&self) -> usize {
        let live = self.windows.iter().map(|w| w.frame_index);
        let pending = self.pending_frame_index.values().copied();
        live.chain(pending)
            .chain(std::iter::once(
                self.saved_window_frames.len().saturating_sub(1),
            ))
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// RFC-005 step 6b — reopen a window the last session had, at the
    /// geometry entry `frame_index` of `window-state.bin`.
    ///
    /// The core drives this: it read the layout file and knows how
    /// many windows there were, while L1 owns the windows and their
    /// ids.  An index past the end of the geometry file (the two files
    /// disagreeing after a crash between writes) is not an error —
    /// the window opens at the default rect and the panes still land.
    fn restore_window(&mut self, frame_index: u32) {
        // Only the FIRST core of this launch restores windows.  A
        // replacement core (silent update, crash respawn) reads the
        // same saved file and asks again — honouring that would stack
        // a second copy of every window on top of the ones already
        // open.  L1 owns windows, so L1 is where that judgement
        // belongs; the core cannot tell whether it is the first one.
        //
        // Keyed on the core generation rather than on "have we
        // restored yet", because a cold launch sends the whole batch
        // in one burst, before any of those windows exist.
        if self.safe_mode {
            lx_warn!(
                "shell.window.restore_refused_safe_mode",
                "safe-mode boot restores no extra windows",
                frame_index = frame_index
            );
            return;
        }
        if self.core_generation > 1 {
            lx_event!(
                "WINDOW_RESTORE_IGNORED",
                "not a cold launch; the windows are already open",
                frame_index = frame_index,
                windows = self.windows.len()
            );
            return;
        }
        let frame = marspot::state::read_windows()
            .and_then(|v| v.into_iter().nth(frame_index as usize))
            .filter(|w| w.w > 50.0 && w.h > 50.0)
            .map(|w| (w.x, w.y, w.w, w.h));
        if frame.is_none() {
            lx_warn!(
                "shell.window.restore_no_frame",
                "no saved geometry for this window; opening at default rect",
                frame_index = frame_index
            );
        }
        self.open_window_with_frame(frame, Some(frame_index as usize));
        lx_event!(
            "WINDOW_RESTORE",
            "core asked for a saved window to be reopened",
            frame_index = frame_index
        );
    }

    /// Allocate an id and ask AppKit for the window.
    ///
    /// Only *asks*.  The window is created after this dispatch returns
    /// (AppKit calls made while the app-state borrow is live panic on
    /// re-entry), and `window_opened` finishes the job.
    fn open_window_with_frame(
        &mut self,
        frame_pt: Option<(f64, f64, f64, f64)>,
        frame_index: Option<usize>,
    ) {
        let window_id = self.next_window_id;
        self.next_window_id = self.next_window_id.wrapping_add(1).max(1);
        let slot = frame_index.unwrap_or_else(|| self.next_frame_index());
        self.pending_frame_index.insert(window_id, slot);
        let attrs = WindowAttrs {
            title: DEFAULT_TITLE.to_string(),
            width_logical: DEFAULT_W_PT,
            height_logical: DEFAULT_H_PT,
            frame_pt,
            bg: marspot::font_cache::BG,
        };
        marspot::app::open_window(window_id, &attrs);
    }

    /// Announce every window past the boot one to a core that has just
    /// been spawned.
    ///
    /// A core learns about the boot window from the env pair it is
    /// spawned with, and about later windows from the
    /// `SurfaceAttachWindow` sent when each opened.  A *replacement*
    /// core (silent update, crash respawn) missed all of those: it
    /// would come up knowing one window, leaving every other window
    /// painting-less forever while its panes were adopted as orphans
    /// into the first one.  Replaying the attaches is what makes a core
    /// swap invisible with more than one window open.
    fn announce_windows_to_new_core(&self, ctx: &MarspotAppCtx) {
        let scale = ctx.scale();
        let lights = ctx.traffic_lights_right_phys();
        for i in 1..self.windows.len() {
            let Some(s) = self.windows[i].surfaces.as_ref() else { continue };
            let (f, b) = s.ids();
            let (w, h) = (s.width() as f64, s.height() as f64);
            // Replay into a fresh core: `ctx` names one window, and
            // its cluster measurement stands in for the rest.  Every
            // window's next resize / attach carries its own.
            self.send_surface_attach(
                self.windows[i].window_id, f, b, w, h, scale, lights,
            );
            lx_event!(
                "WINDOW_REANNOUNCED",
                "replayed a window's surface pair into the new core",
                window_id = self.windows[i].window_id
            );
        }
    }

    /// Index of the window carrying `window_id`.
    fn window_index(&self, window_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.window_id == window_id)
    }

    /// The window whose live or pending pair contains `surface_id`.
    ///
    /// This is why `SurfaceReady` never needed a window id on the
    /// wire: surface ids are global, so the pair that holds one names
    /// its window unambiguously.
    fn window_index_of_surface(&self, surface_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| {
            let has = |p: &Option<SurfacePair>| {
                p.as_ref().is_some_and(|p| {
                    let (f, b) = p.ids();
                    f == surface_id || b == surface_id
                })
            };
            has(&w.surfaces) || has(&w.pending_surfaces)
        })
    }

    /// Announce a surface pair for one window.
    ///
    /// Sends BOTH forms: the legacy window-blind `SurfaceAttach`, which
    /// is the only one a core predating RFC-005 understands, and the
    /// window-aware one.  A new core ignores the legacy frame from the
    /// first window-aware frame onward, so neither peer ever attaches
    /// twice and neither is left without a surface across a skewed
    /// update.  The legacy frame only makes sense for the first window
    /// — it carries no id — so later windows send just the new form.
    fn send_surface_attach(
        &self,
        window_id: u32,
        f: u32,
        b: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
        lights_right_phys: f64,
    ) {
        if window_id == marspot::shell_proto::FIRST_WINDOW_ID {
            self.send(
                MsgType::SurfaceAttach,
                encode_surface_attach(f, b, w_phys, h_phys, scale),
            );
        }
        // The slot rides along so the core can match this window to
        // its saved record by name rather than by arrival order — see
        // `encode_surface_attach_window`.  A window we have not taken
        // on yet (this runs during the replay into a fresh core too)
        // falls back to 0, which the core reads as "no opinion".
        let slot = self
            .window_index(window_id)
            .map(|i| self.windows[i].frame_index)
            .or_else(|| self.pending_frame_index.get(&window_id).copied())
            .unwrap_or(0) as u32;
        self.send(
            MsgType::SurfaceAttachWindow,
            encode_surface_attach_window(
                f, b, w_phys, h_phys, scale, window_id, slot, lights_right_phys,
            ),
        );
    }

    /// Which window an AppKit callback is about.
    ///
    /// The context is the window: `dispatch_event_for` resolved it
    /// from the id the raising view or delegate carries, so a click in
    /// the second window cannot be attributed to the first.
    fn event_window(ctx: &MarspotAppCtx) -> u32 {
        ctx.window_id()
    }

    fn new(proxy: EventProxy) -> Self {
        let binaries = BinaryTree::default_for("marspot-core")
            .expect("HOME must be set to manage binary slots");
        let (pane_badge_tx, pane_badge_rx) = std::sync::mpsc::channel();
        let pane_badge_tx_clone = pane_badge_tx.clone();
        let (pane_title_tx, pane_title_rx) = std::sync::mpsc::channel();
        let pane_title_tx_clone = pane_title_tx.clone();
        let (pane_session_begin_tx, pane_session_begin_rx) = std::sync::mpsc::channel();
        let pane_session_begin_tx_for_ops = pane_session_begin_tx.clone();
        let (pty_op_tx, pty_op_rx) = std::sync::mpsc::channel();
        let (cli_tx, cli_rx) = std::sync::mpsc::channel();
        // Best-effort: a shell that cannot bind still runs the
        // terminal, it just cannot be asked to type into it.
        if let Err(e) = cli_socket::serve(cli_tx) {
            lx_warn!("shell.cli.bind_failed", &format!("{e}"));
        }
        let (inject_input_tx, inject_input_rx) = std::sync::mpsc::channel();
        // The queue's own route to a PTY: the same channel plugins use,
        // so nothing has a private path to a pane.
        let inject_input_tx_for_ops = inject_input_tx.clone();
        let (pane_activity_tx, pane_activity_rx) = std::sync::mpsc::channel();
        Self {
            proxy,
            // The boot window is always entry 0 of both saved lists.
            windows: vec![ShellWindow::new(
                marspot::shell_proto::FIRST_WINDOW_ID,
                0,
            )],
            wake_queue: std::collections::VecDeque::new(),
            wake_next_at: None,
            app_focused: true,
            pending_frame_index: std::collections::HashMap::new(),
            next_window_id: marspot::shell_proto::FIRST_WINDOW_ID + 1,
            active: None,
            redraw_thread_started: false,
            binaries,
            safe_mode: std::env::var("MARSPOT_SAFE_MODE").is_ok(),
            dev_close_extra: false,
            dev_close_sequence: std::env::var("MARSPOT_DEV_CLOSE_SEQUENCE")
                .ok()
                .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
                .unwrap_or_default(),
            dev_close_next_at: None,
            core_generation: 0,
            sup_state: SupervisorState::Idle,
            probe_rx: None,
            crashes: std::collections::VecDeque::new(),
            auto_restart_disabled: false,
            disabled_at: None,
            budget_trips: 0,
            last_tick_at: None,
            banner_kind: None,
            core_boot_ring: std::collections::VecDeque::with_capacity(16),
            plugin_host: {
                let h = ShellPluginHost::new();
                h.attach_pane_badge_tx(pane_badge_tx);
                h.attach_pane_title_tx(pane_title_tx);
                h.attach_pane_session_begin_tx(pane_session_begin_tx);
                h.attach_pty_op_tx(pty_op_tx);
                h.attach_inject_input_tx(inject_input_tx);
                h.attach_pane_activity_tx(pane_activity_tx);
                h
            },
            plugin_registry: PluginRegistry::new(),
            last_plugin_tick: Instant::now() - Duration::from_secs(1),
            pane_recede: std::collections::HashMap::new(),
            pane_status: {
                // Idle is a property of the pane, not of this process:
                // pick up the clocks the previous image left so a
                // silent update doesn't tell every pane it just woke.
                let mut t = pane_status::PaneStateTracker::new();
                let n = t.load_clocks();
                if n > 0 {
                    lx_info!(
                        "shell.pane_state.clocks_restored",
                        "carried pane idle clocks across the restart",
                        panes = n
                    );
                }
                t
            },
            pane_activity_rx,
            // Mirror of `window-state.bin` as it was on disk when this
            // process started.  Load it BEFORE any window opens: the
            // first window's own save must not be allowed to shorten
            // the list past the windows this boot is about to restore.
            saved_window_frames: marspot::state::read_windows().unwrap_or_default(),
            pane_badge_rx,
            pane_title_rx,
            pane_session_begin_rx,
            pty_op_rx,
            cli_rx,
            autorun_panes: cli_socket::load_autorun(),
            autorun_mem: std::collections::HashMap::new(),
            autorun_why: std::collections::HashMap::new(),
            autorun_last_look: None,
            op_host: SupervisorOpHost { begin_tx: pane_session_begin_tx_for_ops },
            pty_ops: plugins::pty_op::PtyOps::new(
                std::sync::Arc::new(plugins::host::HostPtyIo::new(inject_input_tx_for_ops))
                    as std::sync::Arc<dyn plugins::pty_op::PtyIo>,
            ),
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
        match self.active.take() { Some(conn) => {
            let pid = conn.child.as_ref().and_then(|c| Some(c.id())).unwrap_or(0);
            lx_event!(
                "ACTIVE_SHUTDOWN",
                "tearing down active core",
                reason = reason,
                pid = pid
            );
            conn.shutdown();
        } _ => {
            // No-op path is still worth logging — it tells us a
            // shutdown was requested when no core was live (race
            // between supervisor signals).
            lx_debug!(
                "shell.shutdown_active.noop",
                "shutdown_active called but slot was empty",
                reason = reason
            );
        }}
    }

    /// Record a CORE_SPAWN into the rolling ring and flag a
    /// boot-loop alarm if the ring's 30 s window now holds ≥3 boots.
    /// Cheap O(N) on a VecDeque bounded at 16 — the loop check is
    /// already paying VecDeque ops to maintain the ring; the
    /// occasional WARN is dwarfed by everything else in `poll_supervisor`.
    fn record_core_boot(&mut self) {
        self.core_generation = self.core_generation.saturating_add(1);
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

    /// Start applying a pending update: pick the layer, then hand off
    /// to a background Gatekeeper probe.  Nothing is promoted, killed
    /// or spawned here — `finish_probe` does that once the staged
    /// binary has proven it starts.
    ///
    /// History, part 1 — why the swap is single-core.  It used to run a
    /// dual-core probation pattern: active + pending L2 in parallel for
    /// 30 s, presenter atom-swaps to pending if it survives.  That
    /// assumed pending + active could share L3 control connections (the
    /// L4-shelld model where shelld was a multi-consumer broker).
    /// RFC-003 made L3 single-client: as soon as the pending core's UDS
    /// hello reaches L3, L3 closes the previous client (= active core).
    /// Result for the full probation window: active core has dead L3
    /// control sockets, L1 still routes input to active, every
    /// keystroke drops on the floor.  The user sees panes (active still
    /// reads grid shm) but can't type.  So: kill active, spawn new from
    /// promoted current/, let the new core reattach via registry.  L3
    /// self-execv + state.bin reattach means each L3 survives that on
    /// its own; the IOSurface pair is reused, so no presenter handshake
    /// is required either.
    ///
    /// History, part 2 — why the probe came later.  That swap's cost
    /// was budgeted as "the ~200-500 ms gap from old-core-down to
    /// new-core's first SurfaceReady", which held right up until the
    /// gap stopped being bounded.  On 2026-07-29 a cargo build in
    /// another project flooded `syspolicyd`, macOS took 204 s to assess
    /// the freshly staged `marspot-core`, and the swap had already
    /// retired the old one — so the window sat frozen on a dead core's
    /// last frame for three and a half minutes, and relaunching only
    /// queued more cores behind the same stall.  The probe puts that
    /// wait back where it can be afforded: before anything is retired.
    fn apply_pending_update(&mut self) {
        if !matches!(self.sup_state, SupervisorState::Idle) {
            return;
        }
        // Probe every layer that has a candidate in the SAME round.
        // `install-local.sh` stages shell and core together, and
        // handling them as one round is what keeps the core from being
        // started twice: knowing the core is good before we exec means
        // the successor shell spawns it directly, instead of booting
        // the outgoing core and swapping it out seconds later.
        let shell_candidate = supervisor::BinaryTree::for_shell()
            .ok()
            .filter(|t| t.has_pending())
            .map(|t| t.pending());
        let core_candidate = self
            .binaries
            .has_pending()
            .then(|| self.binaries.pending());
        if shell_candidate.is_some() || core_candidate.is_some() {
            self.start_probe(shell_candidate, core_candidate);
        }
    }

    /// Exec a staged binary's `--version` on a background thread so the
    /// kernel charges its Gatekeeper assessment now, while the process
    /// this update is about to replace is still doing its job.
    ///
    /// Nothing here touches `current/`: the candidate is still sitting
    /// in `pending/`.  `promote_pending` moves it with `rename`, which
    /// preserves the inode, so the verdict this warms is the very one
    /// the real spawn will hit.
    ///
    /// Deliberately un-timed.  A slow verdict has to degrade to "the
    /// update lands later", never to "retire the old one anyway and
    /// hope" — that second shape is what froze the window for 204 s on
    /// 2026-07-29.
    fn start_probe(
        &mut self,
        shell_candidate: Option<std::path::PathBuf>,
        core_candidate: Option<std::path::PathBuf>,
    ) {
        lx_event!(
            "UPDATE_PROBE_START",
            "warming staged binaries' Gatekeeper verdicts off the main thread",
            shell = shell_candidate
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            core = core_candidate
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into())
        );
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Sequential on purpose: two concurrent first-execs just
            // queue behind the same Gatekeeper anyway, and serialising
            // keeps the wall-clock attributable per layer in the log.
            let verdict = ProbeVerdict {
                shell: shell_candidate
                    .as_deref()
                    .map(marspot::binary_tree::can_start),
                core: core_candidate
                    .as_deref()
                    .map(marspot::binary_tree::can_start),
            };
            let _ = tx.send(verdict);
        });
        self.probe_rx = Some(rx);
        self.sup_state = SupervisorState::Probing;
    }

    /// Route a finished probe.  A good verdict hands off to that layer's
    /// real swap; a bad one quarantines the candidate — so the next
    /// trigger doesn't retry the same dead binary forever — and leaves
    /// the running process untouched.
    fn finish_probe(&mut self, ctx: &MarspotAppCtx, v: ProbeVerdict) {
        lx_event!("UPDATE_PROBE_DONE", "probe round finished", verdict = v.summary());

        // Quarantine whatever failed, before acting on whatever passed.
        // A rejected candidate must leave `pending/` either way, or the
        // next trigger retries the same dead binary forever.
        if v.shell == Some(false) {
            self.quarantine_rejected("shell", supervisor::BinaryTree::for_shell().and_then(|t| t.quarantine_pending()));
        }
        if v.core == Some(false) {
            self.quarantine_rejected("core", self.binaries.quarantine_pending());
        }

        // A good core promoted BEFORE the exec is the whole point of
        // probing both layers together: the successor shell finds the
        // new binary already in `current/` and spawns it once.  Without
        // this, it boots the outgoing core (still in `current/` at exec
        // time) and a second probe + swap retires it seconds later —
        // two core starts, two L3 control reconnects, for one install.
        if v.shell == Some(true) && v.core == Some(true) {
            match self.binaries.promote_pending() {
                Ok(()) => {
                    lx_event!(
                        "CORE_PROMOTED_FOR_EXECV",
                        "staged core promoted ahead of the shell exec; \
                         the successor spawns it directly"
                    );
                    sup_log::log(
                        "CORE_PROMOTED_FOR_EXECV",
                        "pending/marspot-core → current/ before shell execv",
                    );
                }
                Err(e) => {
                    // Not fatal: the successor boots the old core and
                    // its own trigger swaps it the usual way.
                    lx_warn!(
                        "shell.update.core_preprompte_failed",
                        &format!("{e} — successor will swap it separately")
                    );
                }
            }
        }

        if v.shell == Some(true) {
            // The successor opens ITS window 0 at the frame we hand
            // over, so hand over window 0's frame — not `ctx`'s.  This
            // path runs from the redraw pump, whose ctx is whichever
            // window happened to drive the tick, so with two windows
            // open the boot window could be told to open where the
            // second one was.  `last_saved_window` is window 0's own
            // frame, kept current by its move / resize callbacks.
            let frame0 = self.windows[0]
                .last_saved_window
                .map(|(x, y, w, h, _)| (x, y, w, h))
                .unwrap_or_else(|| ctx.window_frame_pt());
            // Returns only if the exec failed — on success this process
            // is already the new image.
            self.finish_shell_self_update(frame0);
            return;
        }
        if v.core == Some(true) {
            self.perform_core_swap(ctx);
        }
    }

    /// Log one rejected candidate's quarantine outcome.
    fn quarantine_rejected(&self, layer: &str, outcome: std::io::Result<()>) {
        lx_event!(
            "UPDATE_PROBE_REJECT",
            "staged binary could not start — quarantining it, leaving the running one alone",
            layer = layer
        );
        sup_log::log(
            "UPDATE_PROBE_REJECT",
            &format!("pending/{layer} failed its start probe; quarantined"),
        );
        if let Err(e) = outcome {
            lx_warn!(
                "shell.update.quarantine_failed",
                &format!("{e}"),
                layer = layer
            );
        }
    }

    /// The single-core swap itself, reached only once the probe proved
    /// the staged core can start.  From here everything is the fast
    /// path — the Gatekeeper verdict is cached, so the gap between the
    /// old core dying and the new one drawing is back inside the
    /// ~200-500 ms this design was built around.
    fn perform_core_swap(&mut self, ctx: &MarspotAppCtx) -> bool {
        if !self.binaries.has_pending() {
            return false;
        }
        // The core is spawned against the boot window's pair (its ids
        // ride in on env).  Any further window announces itself to the
        // fresh core with a `SurfaceAttachWindow` frame — RFC-005 step
        // 4c, where more than one window can exist.
        let (w_px, h_px, front_id, back_id) = match self.windows[0].surfaces.as_ref() {
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
        self.announce_windows_to_new_core(ctx);
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
    ///
    /// Reached only after `start_probe(ProbeLayer::Shell, …)` came back
    /// good, which is what makes the promotion safe: `exec` replaces
    /// this process, so an image that cannot run means the app is
    /// simply gone — no window, no log line (the redirect at the top of
    /// `main` runs before `logx::init`), every live session orphaned.
    /// That is exactly what happened on 2026-07-26, when an adhoc-signed
    /// binary reached `pending/` and AMFI killed the successor.
    fn finish_shell_self_update(&mut self, window_frame_pt: (f64, f64, f64, f64)) -> bool {
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
        // Every window's surfaces, not just one: anything left behind
        // is inherited by the new image as a dangling fd.
        for w in &mut self.windows {
            if let Some(s) = w.surfaces.take() {
                s.release();
            }
            if let Some(s) = w.pending_surfaces.take() {
                s.release();
            }
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
    /// Note: an update being probed shows **no** banner — the whole
    /// point is invisibility, the active core keeps rendering normally
    /// while the staged binary's Gatekeeper verdict warms up.
    fn refresh_banner(&mut self, ctx: &MarspotAppCtx) {
        // The condition is about the CORE, which every window shares —
        // so the banner goes into every window's presenter, not just
        // whichever one's callback happened to run first.  With one
        // `banner_kind` compared against and one presenter written, the
        // second window silently never showed (or never cleared) it.
        let any_attached = self.windows.iter().any(|w| w.surfaces.is_some());
        let want = if self.safe_mode {
            Some(BannerKind::CrashLoop)
        } else if self.auto_restart_disabled {
            Some(BannerKind::RestartsPaused)
        } else if !self.core_alive() && any_attached {
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
        let scale = ctx.scale();
        for w in self.windows.iter_mut() {
            let Some(p) = w.presenter.as_mut() else { continue };
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
            self.disabled_at = Some(now);
            self.budget_trips = self.budget_trips.saturating_add(1);
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
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        if let Some(s) = self.windows[wi].surfaces.as_ref() {
            let (front_id, back_id) = s.ids();
            let w_px = s.width();
            let h_px = s.height();
            let scale = ctx.scale();
            self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
            if self.active.is_some() {
                self.record_core_boot();
                self.announce_windows_to_new_core(ctx);
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
                        if let Some(stale) = self.windows[wi].pending_surfaces.take() {
                            stale.release();
                        }
                        let (f, b) = pair.ids();
                        self.windows[wi].pending_surfaces = Some(pair);
                        self.send_surface_attach(
                            ctx.window_id(),
                            f,
                            b,
                            w_px as f64,
                            h_px as f64,
                            scale,
                            ctx.traffic_lights_right_phys(),
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

    /// Step every pane's state machine (gated to
    /// `pane_status::SWEEP_INTERVAL` inside the tracker), publish the
    /// result for plugins, and log the committed transitions.
    /// How long between two prefetch wakes.
    ///
    /// One claude starting is a fork, an exec and a transcript read;
    /// sixteen of them together is a stall on the machine the user just
    /// came back to.  A second apart spreads a full window over the
    /// time it takes to read one pane, which is the window that matters
    /// — by the third click the third pane is already up.
    const WAKE_STAGGER: Duration = Duration::from_millis(1_000);

    /// The user is back: line up every parked pane to be woken.
    ///
    /// `MARSPOT_NO_WAKE_PREFETCH=1` turns it off — the wakes then
    /// happen on the click, as they did before.
    fn queue_parked_wakes(&mut self) {
        if std::env::var_os("MARSPOT_NO_WAKE_PREFETCH").is_some()
            || !marspot::settings::get().reclaim_prefetch
        {
            return;
        }
        let parked: Vec<u64> = self
            .active_pane_sessions
            .iter()
            .filter(|(_, a)| a.session.parked())
            .map(|(sid, _)| *sid)
            .collect();
        if parked.is_empty() {
            return;
        }
        for sid in &parked {
            if !self.wake_queue.contains(sid) {
                self.wake_queue.push_back(*sid);
            }
        }
        // First one now; the rest follow on the stagger.
        self.wake_next_at = Some(Instant::now());
        lx_event!(
            "WAKE_PREFETCH",
            "user is back; waking the parked panes ahead of the click",
            panes = parked.len()
        );
    }

    /// Release one queued wake per [`Self::WAKE_STAGGER`].
    fn drive_wake_queue(&mut self) {
        if self.wake_queue.is_empty() {
            return;
        }
        match self.wake_next_at {
            Some(t) if Instant::now() < t => return,
            _ => {}
        }
        let Some(sid) = self.wake_queue.pop_front() else { return };
        self.wake_next_at = Some(Instant::now() + Self::WAKE_STAGGER);
        // Still parked?  The user may have clicked it themselves in the
        // meantime, and waking a run that has moved on is not harmless
        // — it would be typing at whatever is there now.
        if !self
            .active_pane_sessions
            .get(&sid)
            .is_some_and(|a| a.session.parked())
        {
            return;
        }
        self.dispatch_pane_session_focus(sid);
    }

    /// Is this pane's picture currently frozen by a PaneSession?
    ///
    /// The op that asked for the freeze is the one wearing
    /// `FREEZE_GRID`, so the capability is the answer — no second
    /// bookkeeping to keep in step with the first.
    fn pane_picture_held(&self, sid: u64) -> bool {
        self.active_pane_sessions.get(&sid).is_some_and(|s| {
            s.session.caps() & marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID != 0
        })
    }

    fn sweep_pane_status(&mut self) {
        // None = interval gate; the machines weren't stepped.
        let now = Instant::now();
        let Some(changes) = self.pane_status.sweep(now) else {
            return;
        };
        for c in &changes {
            // INFO, not DEBUG: the runtime default level is Info, so a
            // DEBUG line does not exist on a real machine — and the
            // transition history is what any future threshold gets
            // calibrated against.  Rate is capped by construction: one
            // line per pane per committed transition, and a pane that
            // sits quiet for an hour prints once.
            lx_info!(
                "shell.pane_state.changed",
                "pane state machine committed a transition",
                sid = c.sid,
                from = c.change.from.label().as_str(),
                held_s = c.change.held.as_secs(),
                to = c.change.to.label().as_str()
            );
        }
        let snapshot = self.pane_status.snapshot(now);
        // Tell L2 how each pane should look.  On change only: a pane
        // that has been resting for an hour costs one frame, not one
        // per second.
        for (sid, (status, held, _)) in &snapshot {
            // A pane whose picture is held keeps the brightness it was
            // held at.  The hold stops the *cells* moving, not the byte
            // traffic underneath — reclaiming a pane kills the program
            // in it, which produces output, which reads as "busy" and
            // pulled the pane to full brightness, then to dormant a few
            // seconds later.  Observed 2026-08-03: content perfectly
            // still, pane visibly flashing bright and then dimming two
            // steps.  Everything drawn around a frozen picture has to
            // be frozen with it or the pane announces what is being
            // done to it.
            if self.pane_picture_held(*sid) {
                continue;
            }
            let level = pane_status::recede_level_for(status, *held);
            if self.pane_recede.get(sid).copied() != Some(level) {
                self.pane_recede.insert(*sid, level);
                self.send(
                    MsgType::PaneRecede,
                    marspot::shell_proto::encode_pane_recede(*sid, level),
                );
                // On change only — a pane that stays rested for an
                // hour is one line, not one per second.  Worth having:
                // "the pane looks wrong" is otherwise unanswerable
                // without a screenshot.
                lx_info!(
                    "shell.pane_recede.changed",
                    "pane recede level changed",
                    sid = *sid,
                    level = level as u64,
                    state = status.label().as_str(),
                    held_s = held.as_secs()
                );
            }
        }
        self.pane_recede.retain(|sid, _| snapshot.contains_key(sid));
        self.plugin_host.publish_pane_status(snapshot);
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
        // Order matters twice over: drain the plugins' activity
        // reports first so this round's observation is built from
        // them, then step the machines, and only then tick the plugins
        // — which read the fresh state back through the host.
        while let Ok((sid, activity)) = self.pane_activity_rx.try_recv() {
            self.pane_status.report_activity(sid, activity);
        }
        // One `stat` a tick.  This is what makes the settings file
        // live: edit it by hand or from the panel and the next sweep
        // is already using the new values, with nothing restarted.
        if marspot::settings::reload_if_changed() {
            let s = marspot::settings::get();
            lx_event!(
                "SETTINGS_RELOADED",
                "settings.toml changed on disk",
                reclaim = s.reclaim_enabled as u32,
                idle_min = s.reclaim_idle_minutes as u64,
                prefetch = s.reclaim_prefetch as u32
            );
        }
        self.sweep_pane_status();
        self.sweep_autorun();
        self.dev_drive_close_sequence();
        self.drive_wake_queue();
        self.last_plugin_tick = Instant::now();
        self.plugin_registry.tick_all_with(&self.plugin_host);

        // Answer the command socket.  Before the submit drain, so a
        // request that arrives this pass runs this pass.
        while let Ok(req) = self.cli_rx.try_recv() {
            match req {
                cli_socket::CliRequest::SendText { target, text, reply } => {
                    let (ok, msg) = self.run_cli_send(&target, &text);
                    lx_info!(
                        "shell.cli.send",
                        &msg,
                        target = target.as_str(),
                        bytes = text.len() as u32,
                        ok = ok as u32
                    );
                    let _ = reply.send((ok, msg));
                }
                cli_socket::CliRequest::ListPanes { reply } => {
                    let _ = reply.send(Self::addressed_panes());
                }
                cli_socket::CliRequest::Autorun { target, on, reply } => {
                    let out = self.run_cli_autorun(&target, on);
                    lx_info!(
                        "shell.autorun.switch",
                        match &out {
                            Ok(t) => t.clone(),
                            Err(e) => e.clone(),
                        }
                        .as_str(),
                        target = target.as_str()
                    );
                    let _ = reply.send(out);
                }
                cli_socket::CliRequest::ReadPane { target, extra_lines, reply } => {
                    let out = self.run_cli_read(&target, extra_lines);
                    lx_info!(
                        "shell.cli.read",
                        match &out {
                            Ok(t) => format!("{} bytes", t.len()),
                            Err(e) => e.clone(),
                        }
                        .as_str(),
                        target = target.as_str(),
                        ok = out.is_ok() as u32
                    );
                    let _ = reply.send(out);
                }
            }
        }

        // Take in whatever was submitted, then start what can start and
        // collect what finished.  Before `process_pane_sessions` so a
        // run that starts this round gets its first tick immediately.
        while let Ok(req) = self.pty_op_rx.try_recv() {
            if self
                .pty_ops
                .submit_at(req.shelld_session_id, req.op, req.start_at)
                .is_none()
            {
                lx_warn!(
                    "shell.pty_op.queue_full",
                    "dropping a submitted operation",
                    shelld_session_id = req.shelld_session_id
                );
            }
        }
        for r in self.pty_ops.pump(&self.op_host) {
            if !r.outcome.is_done() {
                lx_warn!(
                    "shell.pty_op.not_done",
                    &format!("{:?}", r.outcome),
                    op = r.name,
                    shelld_session_id = r.sid
                );
            }
        }

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
                match req.what {
                    plugins::host::InjectWhat::Input(bytes) => conn.send(
                        MsgType::InjectInput,
                        marspot::shell_proto::encode_inject_input(
                            req.session_id,
                            &bytes,
                        ),
                    ),
                    plugins::host::InjectWhat::HoldGrid(on) => conn.send(
                        MsgType::PaneHoldGrid,
                        marspot::shell_proto::encode_pane_hold_grid(
                            req.session_id,
                            on,
                        ),
                    ),
                    plugins::host::InjectWhat::Paste(text) => conn.send(
                        MsgType::PaneInjectPaste,
                        marspot::shell_proto::encode_pane_inject_paste(
                            req.session_id,
                            &text,
                        ),
                    ),
                }
            }
        }

        // 0a. Were *we* running?  Everything below judges the core by
        // how long it has been quiet, which is only evidence if
        // somebody was listening the whole time.  When the gap between
        // two ticks blows past `SUPERVISOR_STALL_GAP` the shell itself
        // was descheduled — the machine slept, or a build storm took
        // the cores — and the core's deadlines get that time handed
        // back instead of charged.  `next_ping_at` is deliberately
        // left alone: the next tick pings immediately, which is how we
        // find out what actually happened.
        let tick_now = Instant::now();
        let since_last = self.last_tick_at.map(|t| tick_now.duration_since(t));
        self.last_tick_at = Some(tick_now);
        if let Some(tick_gap) = since_last.filter(|g| *g > SUPERVISOR_STALL_GAP) {
            if let Some(c) = self.active.as_mut() {
                c.spawned_at = forgive_stall(c.spawned_at, tick_gap, tick_now);
                c.last_pong_at = forgive_stall(c.last_pong_at, tick_gap, tick_now);
            }
            lx_event!(
                "SUPERVISOR_STALL",
                "supervisor tick was late — deadlines forgiven, not charged",
                gap_ms = tick_gap.as_millis() as u64
            );
        }

        // 0b. Cool-down after a blown crash budget.  Not "until manual
        // intervention" any more: the four restarts that trip the
        // budget are usually a machine under load, and the load
        // passes.  Waiting one `CRASH_WINDOW` before the next attempt
        // keeps the restart *rate* bounded — the whole point of the
        // budget — without leaving the window frozen behind a banner
        // for the rest of the day.
        let cooldown = restart_cooldown(self.budget_trips);
        if self.auto_restart_disabled
            && self.disabled_at.is_some_and(|t| t.elapsed() >= cooldown)
        {
            lx_event!(
                "BUDGET_RECOVERED",
                "cool-down elapsed; trying the core once more",
                trips = self.budget_trips,
                cooldown_s = cooldown.as_secs()
            );
            sup_log::log(
                "BUDGET_RECOVERED",
                &format!("trips={} cooldown_s={}", self.budget_trips, cooldown.as_secs()),
            );
            self.auto_restart_disabled = false;
            self.disabled_at = None;
            self.crashes.clear();
            self.restart_core(ctx);
            self.refresh_banner(ctx);
            return;
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
            .map(|c| pong_overdue(c.hello_acked, c.last_pong_at, now))
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

        // 4b. Collect an in-flight Gatekeeper probe.  Nothing about the
        // outgoing process is disturbed while this is pending — it keeps
        // rendering and keeps taking input — so a probe that takes
        // minutes costs a late update, not a frozen window.
        if matches!(self.sup_state, SupervisorState::Probing) {
            match self.probe_rx.as_ref().map(mpsc::Receiver::try_recv) {
                // Still exec'ing.  Leave the state alone; we'll ask again
                // next tick.
                Some(Err(mpsc::TryRecvError::Empty)) => {}
                got => {
                    // A real answer, or the thread died without sending
                    // (Disconnected) / no channel at all.  Both of the
                    // latter mean "we did not prove anything can start",
                    // which is the same conclusion as a failed probe —
                    // and `ProbeVerdict::default()` is all-`None`, so
                    // nothing gets promoted or quarantined off it.
                    let verdict = match got {
                        Some(Ok(v)) => v,
                        _ => supervisor::ProbeVerdict::default(),
                    };
                    self.probe_rx = None;
                    self.sup_state = SupervisorState::Idle;
                    self.finish_probe(ctx, verdict);
                }
            }
        }

        // 5. Manual update trigger via SIGUSR1.  Lets a CLI invoke
        // `kill -USR1 $(pgrep marspot-shell)` to apply a staged
        // update on demand instead of waiting for focus-loss.
        if SIGUSR1_FLAG.swap(false, Ordering::AcqRel) {
            sup_log::log("SIGUSR1", "manual update trigger");
            if matches!(self.sup_state, SupervisorState::Idle) {
                self.apply_pending_update();
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
        // Surface ids are global, so the pair holding this one names
        // its window — no window id is needed on the wire.
        let Some(i) = self.window_index_of_surface(id) else {
            lx_warn!(
                "shell.surface_ready.id_unknown",
                "SurfaceReady ignored — id belongs to no window's pair",
                id = id
            );
            return;
        };
        let live_ids = self.windows[i].surfaces.as_ref().map(|p| p.ids());
        let pending_ids = self.windows[i].pending_surfaces.as_ref().map(|p| p.ids());
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
            if let Some(p) = self.windows[i].presenter.as_mut() {
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
            self.windows[i].first_frame_ready = true;
            self.windows[i].frame_pending = true;
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
        let new_pair = match self.windows[i].pending_surfaces.take() {
            Some(p) => p,
            None => return,
        };
        match self.windows[i].presenter.as_mut() {
            Some(p) => {
                if let Err(e) = p.set_pair(&new_pair.front, &new_pair.back) {
                    lx_error!("shell.set_pair_failed", &format!("{e}"));
                    new_pair.release();
                    return;
                }
                // The acked id is the one the core just wrote — point at it.
                p.swap_to_id(id);
            }
            None => {
                // Silently skipping here is how the 2026-07-28 black
                // window stayed invisible in the logs: the pair got
                // installed, `pair_swapped` was logged, and nothing on
                // screen ever changed.  Every window must have its
                // presenter by the time its first frame is acked.
                lx_error!(
                    "shell.surface_ready.no_presenter",
                    "pair acked but this window has no presenter — it will render black",
                    window_id = self.windows[i].window_id,
                    id = id
                );
            }
        }
        if let Some(old) = self.windows[i].surfaces.take() {
            old.release();
        }
        lx_info!(
            "shell.presenter.pair_swapped",
            "presenter now displaying new pair",
            front = new_pair.front.id(),
            back = new_pair.back.id(),
            acked = id
        );
        self.windows[i].surfaces = Some(new_pair);
        self.windows[i].first_frame_ready = true;
        self.windows[i].frame_pending = true;
        self.dev_close_extra_if_armed(i);
    }

    /// Shut marspot down.
    ///
    /// **The running sessions are deliberately left alone.**  Quitting
    /// is putting marspot away, not ending the work in it: every L3
    /// keeps its PTY, its shell and whatever is running inside, and
    /// the next launch reattaches to them from the registry.  That is
    /// what makes "close the windows, come back tomorrow" keep the
    /// sessions — and it has to hold whichever window was closed last,
    /// which is why closing a window parks it rather than retiring it.
    ///
    /// This used to SIGTERM every L3 on the way out (RFC-003 §6
    /// Amendment 15, "user-driven quit = clean account").  The panes
    /// came back — reincarnated from state.bin + bytelog — but as new
    /// shells in the old directory, so anything actually running in
    /// them was gone.
    ///
    /// L3s that outlive their usefulness are still collected: one that
    /// no window claims is retired by the next boot's assembly sweep,
    /// and one whose session dir goes away exits on its own.
    fn quit(&mut self, ctx: &MarspotAppCtx) {
        // RFC-001: clean plugin shutdown FIRST, so plugins releasing
        // host resources (notifications, fs watchers) don't race
        // against the rest of the teardown.
        self.plugin_registry.stop_all_with(&self.plugin_host);

        // RFC-003 §6 Amendment 15.2 — pgrep sweep for orphan L3s.
        // Resurrect / silent-update spawn races can leave behind L3
        // processes whose entry.toml got overwritten by a later
        // sibling: unreachable, so nothing will ever reattach to them.
        // Registered sessions are skipped — those are the ones we are
        // keeping.
        let entries = marspot_term::session_registry::list_session_entries();
        let known_pids: std::collections::HashSet<i32> =
            entries.iter().map(|e| e.pid).collect();
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
        lx_event!(
            "SHELL_QUIT_CLEANUP",
            "quitting; registered sessions kept running, orphans swept",
            n_kept = entries.len(),
            n_orphans_killed = orphans_killed
        );

        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.shutdown_active("window close_requested");
        // App quit: release every window's surfaces, not just the one
        // whose close button was pressed.
        for w in &mut self.windows {
            if let Some(pair) = w.surfaces.take() {
                pair.release();
            }
            if let Some(stale) = w.pending_surfaces.take() {
                stale.release();
            }
        }
        ctx.exit();
    }

    /// Close ONE window: tell the core, drop our side, ask AppKit.
    ///
    /// Extracted from `close_requested` so the dev seam that a test
    /// uses runs this exact code rather than a second copy of it —
    /// the bug it guards against (over-released NSWindow) lives in
    /// what happens after `app::close_window`, so a paraphrase would
    /// not have caught it.
    fn close_window_by_id(&mut self, window_id: u32, discard: bool) {
        // Tell the core first: it parks or discards the window while
        // the surfaces are still alive, so nothing renders into a
        // released pair on the way out.
        self.send(
            MsgType::WindowClosed,
            marspot::shell_proto::encode_window_closed(window_id),
        );
        self.pending_frame_index.remove(&window_id);
        if let Some(i) = self.window_index(window_id) {
            let w = self.windows.remove(i);
            if discard {
                self.forget_slot(w.frame_index);
            }
            if let Some(p) = w.surfaces {
                p.release();
            }
            if let Some(p) = w.pending_surfaces {
                p.release();
            }
        }
        // Queued: the NSWindow is torn down after this dispatch,
        // for the same re-entrancy reason opening one is.
        marspot::app::close_window(window_id);
        lx_event!(
            "WINDOW_CLOSED",
            "closed one window; app keeps running",
            window_id = window_id,
            shell_windows = self.windows.len()
        );
    }

    /// Dev seam (`MARSPOT_DEV_CLOSE_EXTRA`): close this window again,
    /// now that it has a painted pair on screen, through the very path
    /// the red button takes.  Hooked here rather than in `redraw`
    /// because a background window may never get a draw callback at
    /// all — the pump is poke-driven, and the test's app is not
    /// frontmost.
    fn dev_close_extra_if_armed(&mut self, i: usize) {
        if !self.windows[i].dev_close_after_paint {
            return;
        }
        self.windows[i].dev_close_after_paint = false;
        let window_id = self.windows[i].window_id;
        lx_event!(
            "DEV_CLOSE_EXTRA",
            "pressing this window's close button",
            window_id = window_id
        );
        // `performClose:`, not our own close path: the delegate then
        // calls back into `close_requested` while AppKit is still
        // inside its close machinery, which is the shape the red
        // button has and the shape that crashed.
        marspot::app::perform_close(window_id);
    }

    /// Dev seam (`MARSPOT_DEV_CLOSE_SEQUENCE`): work through the list,
    /// one red button every couple of seconds, starting once every
    /// window named in it has painted.  The last entry is the last
    /// window, so the sequence ends in a real quit.
    fn dev_drive_close_sequence(&mut self) {
        if self.dev_close_sequence.is_empty() {
            return;
        }
        let Some(due) = self.dev_close_next_at else {
            let all_up = self.dev_close_sequence.iter().all(|id| {
                self.windows
                    .iter()
                    .any(|w| w.window_id == *id && w.first_frame_ready)
            });
            if all_up {
                self.dev_close_next_at =
                    Some(Instant::now() + Duration::from_secs(2));
            }
            return;
        };
        if Instant::now() < due {
            return;
        }
        let window_id = self.dev_close_sequence.pop_front().unwrap_or(0);
        self.dev_close_next_at = Some(Instant::now() + Duration::from_secs(2));
        lx_event!(
            "DEV_CLOSE_SEQUENCE",
            "pressing this window's close button (dev seam)",
            window_id = window_id,
            remaining = self.dev_close_sequence.len()
        );
        marspot::app::perform_close(window_id);
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
                        // The PONG deadline starts **here**, not at
                        // spawn.  `last_pong_at` is seeded at spawn as
                        // a freebie, but booting is not free: attach
                        // the surfaces, reattach thirteen L3s, build
                        // the layout.  On a loaded machine that took
                        // 20 s, so the moment this core became able to
                        // answer, its deadline had already expired and
                        // step 4 killed it in the same tick — 184 ms
                        // after the handshake it had just completed.
                        // The replacement hit the same wall, and the
                        // one after that, until the crash budget
                        // tripped and the window bricked behind
                        // "please restart the app"
                        // (2026-08-11T13:21–13:22Z, four restarts, one
                        // signature).  A deadline that starts before
                        // the peer can reply measures our boot, not
                        // its health.
                        let now = Instant::now();
                        c.last_pong_at = now;
                        c.next_ping_at = now + PING_INTERVAL;
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
            ShellInbox::CaretRect(rect, window_id) => {
                // NOT `ctx` — the control socket is drained in
                // `user_event`, which is an event about the process and
                // so always dispatched against the first window.  Using
                // `ctx` sent every window's caret to window 1's view;
                // the others answered `firstRectForCharacterRange:` with
                // a zero rect and the IME candidate window detached from
                // the caret.  The id on the wire names the right one.
                marspot::app::set_caret_rect_phys_for(window_id, rect);
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
                if let Some(wi) = self.window_index(ctx.window_id()) {
                    self.windows[wi].first_frame_ready = true;
                    self.windows[wi].frame_pending = true;
                }
            }
            ShellInbox::PaneBadgeClicked(shelld_sid) => {
                self.plugin_registry
                    .dispatch_pane_badge_click_with(&self.plugin_host, shelld_sid);
            }
            ShellInbox::PaneBadgeMenuRequest(shelld_sid, x, y) => {
                let items = self.plugin_registry.dispatch_pane_badge_menu(
                    &self.plugin_host,
                    shelld_sid,
                );
                // Empty menu still gets no reply on purpose — L2
                // opens nothing either way, so the frame would be
                // dead weight.
                if !items.is_empty() {
                    if let Some(conn) = self.active.as_ref() {
                        conn.send(
                            MsgType::PaneBadgeMenu,
                            marspot::shell_proto::encode_pane_badge_menu(
                                shelld_sid, x, y, &items,
                            ),
                        );
                    }
                }
            }
            ShellInbox::PaneBadgeMenuAction(shelld_sid, tag) => {
                self.plugin_registry.dispatch_pane_badge_menu_action(
                    &self.plugin_host,
                    shelld_sid,
                    tag,
                );
            }
            ShellInbox::PaneSessionKey(sid, ev) => {
                self.dispatch_pane_session_key(sid, ev);
            }
            ShellInbox::PaneFocused(sid) => {
                // Straight to the pane's own session, if it has one:
                // a reclaimed pane starts restoring the moment the
                // user looks at it, instead of after they type.
                self.dispatch_pane_session_focus(sid);
                self.plugin_registry
                    .dispatch_pane_focused(&self.plugin_host, sid);
            }
            ShellInbox::PaneSessionUserEscape(sid) => {
                self.end_pane_session(sid, plugins::EndReason::UserEscape);
            }
            ShellInbox::WindowOpenRequest(frame_index, hint) => {
                if frame_index == marspot::shell_proto::WINDOW_OPEN_USER {
                    // A user action (move-pane-to-new-window), not a
                    // boot restore: none of the restore gates apply —
                    // safe mode blocks automatic multiplication, and
                    // an explicit user request is neither automatic
                    // nor a spawn (the pane already exists).
                    //
                    // RFC-006 §4 — a drag-to-desktop carries the
                    // release point (screen pts): the window opens
                    // centred there, so it is born where the pane was
                    // dropped.  AppKit's own constrain pass keeps it
                    // on-screen.
                    let frame = hint.map(|(cx, cy)| {
                        (
                            cx - DEFAULT_W_PT / 2.0,
                            cy - DEFAULT_H_PT / 2.0,
                            DEFAULT_W_PT,
                            DEFAULT_H_PT,
                        )
                    });
                    self.open_window_with_frame(frame, None);
                } else {
                    self.restore_window(frame_index);
                }
            }
            ShellInbox::WindowCloseRequest(window_id) => {
                // The core asked because this window emptied out — its
                // last pane was closed, or moved away.  Either way
                // there is nothing left in it.
                //
                // When it is the last window, an empty window means an
                // empty app: the user closed the final pane, and
                // marspot goes with it.  The core has already dropped
                // the saved layout; drop the saved geometry to match,
                // so the next launch opens a default window instead of
                // reusing the frame of what was just dismantled.
                if marspot::app::window_count() > 1 {
                    // The core only asks when the window emptied out:
                    // its panes are gone for good, so its slot goes too.
                    self.close_window_by_id(window_id, true);
                } else {
                    lx_event!(
                        "WINDOW_CLOSE_LAST_PANE",
                        "last pane of the last window closed — quitting",
                        window_id = window_id
                    );
                    if let Err(e) = marspot::state::clear_windows() {
                        lx_warn!("shell.window_state.clear_failed", &format!("{e}"));
                    }
                    self.saved_window_frames.clear();
                    self.quit(ctx);
                }
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

    /// Every live pane as `(session id, cwd, title)`.
    ///
    /// The cwd is read live off the pane's shell rather than taken from
    /// the registry: the registry's copy is from spawn time and says
    /// `/Users/doracawl` for every pane that has since cd'd somewhere,
    /// which is all of them.
    fn live_panes() -> Vec<(u64, String, String)> {
        marspot_term::session_registry::list_session_entries()
            .into_iter()
            .map(|e| {
                let cwd = marspot::pidtree::proc_cwd(e.shell_child_pid)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(e.cwd);
                (e.id, cwd, e.title)
            })
            .collect()
    }

    /// Every pane with the two things naming needs: its directory and
    /// where it sits.  One place builds this, so the title strip, the
    /// listing and `--send` cannot disagree about what `spg#2` means.
    fn pane_refs() -> Vec<marspot::pane_name::PaneRef> {
        let cells = Self::pane_cells();
        Self::live_panes()
            .into_iter()
            .map(|(sid, cwd, _)| {
                // `pane_cells` reports (window, x, y); naming sorts in
                // reading order, which is (window, y, x).
                let at = cells.get(&sid).map(|(w, x, y)| (*w, *y, *x));
                marspot::pane_name::PaneRef::new(sid, cwd, at)
            })
            .collect()
    }

    /// Every pane as `(id, cwd, address)` — name and cell both, so a
    /// listing answers "how do I say this one again?" without the
    /// reader having to work out the numbering.
    fn addressed_panes() -> Vec<(u64, String, String)> {
        let refs = Self::pane_refs();
        let named: std::collections::HashMap<u64, String> =
            cli_socket::assign_names(&refs).into_iter().collect();
        let cells = Self::pane_cells();
        refs.into_iter()
            .map(|marspot::pane_name::PaneRef { sid, cwd, .. }| {
                let name = named.get(&sid).cloned().unwrap_or_default();
                let addr = match cells.get(&sid) {
                    Some((w, x, y)) => format!("{name}  w({w},{x},{y})"),
                    None => name,
                };
                (sid, cwd, addr)
            })
            .collect()
    }

    /// `sid → (window, x, y)`, 1-based, from the layout L2 persists.
    fn pane_cells() -> std::collections::HashMap<u64, (usize, usize, usize)> {
        let mut out = std::collections::HashMap::new();
        let Some(saved) = marspot::state::read() else {
            return out;
        };
        for (wi, win) in saved.windows.iter().enumerate() {
            let cols = win.grid_cols.max(1) as usize;
            for (i, p) in win.panes.iter().enumerate() {
                if p.sid != 0 {
                    out.insert(p.sid, (wi + 1, i % cols + 1, i / cols + 1));
                }
            }
        }
        out
    }

    /// Switch the policy for a pane, or report where it is on.
    ///
    /// Stored by working directory: that is what "the pane for this
    /// project" means, and it is the one identity that survives a
    /// restart, a move between windows, and a twin appearing (which
    /// changes the pane's name).
    fn run_cli_autorun(&mut self, target: &str, on: Option<bool>) -> Result<String, String> {
        if target.trim().is_empty() {
            if self.autorun_panes.is_empty() {
                return Ok("autorun is off everywhere".into());
            }
            let mut on: Vec<&String> = self.autorun_panes.iter().collect();
            on.sort();
            return Ok(on
                .iter()
                .map(|cwd| format!("{cwd}\n"))
                .collect::<String>()
                .trim_end()
                .to_string());
        }
        let sid = self.resolve_target(target)?;
        let cwd = Self::live_panes()
            .into_iter()
            .find(|(s, _, _)| *s == sid)
            .map(|(_, cwd, _)| cwd)
            .ok_or_else(|| format!("pane {sid} is gone"))?;
        match on {
            Some(true) => {
                self.autorun_panes.insert(cwd.clone());
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun on for {cwd} (pane {sid})"))
            }
            Some(false) => {
                self.autorun_panes.remove(&cwd);
                self.autorun_mem.remove(&sid);
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun off for {cwd}"))
            }
            None => Ok(if self.autorun_panes.contains(&cwd) {
                format!("autorun is on for {cwd} (pane {sid})")
            } else {
                format!("autorun is off for {cwd}")
            }),
        }
    }

    /// How often the autorun policy looks at its panes.
    ///
    /// Deliberately slow.  Every trigger it can fire on requires the
    /// pane to have been quiet for longer than this already, so looking
    /// more often could only make it act sooner on evidence it should
    /// be sure of.
    const AUTORUN_INTERVAL: Duration = Duration::from_secs(30);

    /// Look at every pane the policy is on for, and act if it says to.
    fn sweep_autorun(&mut self) {
        if self.autorun_panes.is_empty() {
            return;
        }
        let now = Instant::now();
        if self
            .autorun_last_look
            .is_some_and(|t| now.duration_since(t) < Self::AUTORUN_INTERVAL)
        {
            return;
        }
        self.autorun_last_look = Some(now);

        // The state machine's own view — the same one the reclamation
        // policy acts on, so "quiet" means one thing in this process.
        let statuses = self.pane_status.snapshot(now);
        let wall = std::time::SystemTime::now();
        let panes: Vec<(u64, String)> = Self::live_panes()
            .into_iter()
            .filter(|(_, cwd, _)| self.autorun_panes.contains(cwd))
            .map(|(sid, cwd, _)| (sid, cwd))
            .collect();
        self.autorun_mem.retain(|sid, _| panes.iter().any(|(s, _)| s == sid));

        for (sid, cwd) in panes {
            let Some((status, _held, _quiescent)) = statuses.get(&sid) else { continue };
            // The screen, as a person would read it.  Cheap enough at
            // this cadence (one file tail + a replay) and the only way
            // to see what the session actually said.
            let screen = Self::read_pane_screen(sid).unwrap_or_default();
            let look = plugins::autorun::Look {
                // Exactly one state is a pane worth typing into: a
                // bound session that finished its turn.  Empty, parked,
                // contradictory and unknown are all quiet too, and
                // typing `/clear` at any of them is a shell command
                // that does not exist.
                awaiting_user: matches!(
                    status,
                    marspot::pane_state::PaneStatus::AwaitingUser
                ),
                // `Busy` covers both halves of "it is doing something":
                // a job the shell is holding, and a program that is
                // still drawing.  A rotation is not over while its
                // build is running, however quiet the terminal looks.
                work_in_flight: matches!(status, marspot::pane_state::PaneStatus::Busy(_)),
                screen: &screen,
            };
            let mem = self.autorun_mem.entry(sid).or_default();
            let was_exhausted = mem.is_exhausted();
            let (action, why) = plugins::autorun::decide(&look, mem, wall);
            if mem.is_exhausted() && !was_exhausted {
                lx_warn!(
                    "shell.autorun.exhausted",
                    "no response after every retry; leaving this pane alone",
                    shelld_session_id = sid,
                    cwd = cwd.as_str()
                );
            }
            if let Err(waiting) = why {
                // Say why, once per change of reason.  Without this the
                // log is silent until something fires, and "nothing
                // happened" reads the same whether the policy is
                // waiting or wedged.  That question came back on day
                // one — *why has torajs stopped?* — and there was
                // nothing to answer it with.
                let reason = format!("{waiting:?}");
                if self.autorun_why.get(&sid) != Some(&reason) {
                    lx_info!(
                        "shell.autorun.waiting",
                        &reason,
                        shelld_session_id = sid,
                        cwd = cwd.as_str()
                    );
                    self.autorun_why.insert(sid, reason);
                }
                continue;
            }
            self.autorun_why.remove(&sid);
            let lines: Vec<&str> = match action {
                plugins::autorun::Action::ClearAndContinue => {
                    vec!["/clear", plugins::autorun::CONTINUE_AUTORUN]
                }
                plugins::autorun::Action::Continue => vec![plugins::autorun::CONTINUE],
                // The line is already in the box; all it needs is the
                // Enter that went missing.
                plugins::autorun::Action::SubmitPending => vec![""],
                plugins::autorun::Action::Nothing => continue,
            };
            lx_info!(
                "shell.autorun.act",
                &format!("{why:?} → {lines:?}"),
                shelld_session_id = sid,
                cwd = cwd.as_str(),
                attempt = mem.attempts()
            );
            if let Err(e) = self.autorun_type(sid, &lines) {
                lx_warn!("shell.autorun.send_failed", &e, shelld_session_id = sid);
            }
        }
    }

    /// Type each line into the pane, in order, as one operation.
    ///
    /// One operation, not one per line: the queue serialises whole
    /// operations, so splitting them would let something else in
    /// between `/clear` and what follows it.  Each line waits for the
    /// pane to draw before the next goes in — `/clear` restarts the
    /// session's UI, and a line typed into that gap lands nowhere.
    fn autorun_type(&mut self, sid: u64, lines: &[&str]) -> Result<(), String> {
        if self.pty_ops.is_busy(sid) {
            return Err(format!(
                "pane {sid} is busy ({})",
                self.pty_ops.running(sid).unwrap_or("?")
            ));
        }
        let mut op = plugins::pty_op::PtyOp::new("autorun")
            // A delivery, not a takeover: the pane is not frozen and
            // the keyboard is not locked, so a person who walks up
            // mid-sequence keeps control of their own session.
            .lock_keys(false);
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                // Just the Enter: the text is already in the box.
                op = op.step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
                continue;
            }
            if i > 0 {
                // Let the previous line land and the UI settle before
                // the next one.  Typing `继续 autorun` into a session
                // that is still processing `/clear` loses it.
                op = op
                    .step(plugins::pty_op::Step::await_quiet(Duration::from_millis(700))
                        .timeout(Duration::from_secs(20)));
            }
            op = op
                .step(plugins::pty_op::Step::paste(*line))
                .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        }
        self.pty_ops
            .submit(sid, op)
            .map(|_| ())
            .ok_or_else(|| format!("pane {sid} has too much queued"))
    }

    /// The pane's screen, replayed from its bytelog.
    fn read_pane_screen(sid: u64) -> Option<String> {
        let entry = marspot_term::session_registry::list_session_entries()
            .into_iter()
            .find(|e| e.id == sid)?;
        let bytelog = marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog");
        marspot::pane_read::screen_text(&bytelog, entry.cols, entry.rows, 0).ok()
    }

    /// Turn what a caller said into one pane.
    ///
    /// Three forms, because the useful address depends on what the
    /// caller knows: a **name** when it knows the project, an **id**
    /// when it was handed one, and a **cell** when it means "whatever
    /// is in that slot of that window".
    fn resolve_target(&self, target: &str) -> Result<u64, String> {
        let panes = Self::pane_refs();
        match cli_socket::parse_target(target)? {
            cli_socket::Target::Id(sid) => {
                if panes.iter().any(|p| p.sid == sid) {
                    Ok(sid)
                } else {
                    Err(format!("no pane with session id {sid}"))
                }
            }
            cli_socket::Target::Name(n) => cli_socket::resolve_name(&n, &panes),
            cli_socket::Target::Cell { w, x, y } => Self::pane_at_cell(w, x, y),
        }
    }

    /// The pane occupying window `w`'s cell `(x, y)`, all 1-based.
    ///
    /// Read from the layout L2 persists on every spawn / close / focus
    /// change / layout apply, which is the only place the arrangement
    /// is known — L1 owns the windows, L2 decides what sits where.
    fn pane_at_cell(w: usize, x: usize, y: usize) -> Result<u64, String> {
        let saved = marspot::state::read()
            .ok_or_else(|| "no window layout on disk yet".to_string())?;
        let win = saved
            .windows
            .get(w - 1)
            .ok_or_else(|| format!("there is no window {w} (there are {})", saved.windows.len()))?;
        let (cols, rows) = (win.grid_cols as usize, win.grid_rows as usize);
        if x > cols || y > rows {
            return Err(format!("window {w} is {cols}×{rows}; ({x},{y}) is outside it"));
        }
        let idx = (y - 1) * cols + (x - 1);
        match win.panes.get(idx) {
            Some(p) if p.sid != 0 => Ok(p.sid),
            _ => Err(format!("window {w} cell ({x},{y}) is empty")),
        }
    }

    /// What the pane called `target` says.
    ///
    /// Replayed from its bytelog rather than asked of L3: the record is
    /// already on disk and complete, so reading costs one file tail and
    /// disturbs nothing — no round trip, no state, and a pane that is
    /// mid-reclamation answers as readily as one nobody has touched.
    fn run_cli_read(&self, target: &str, extra_lines: u32) -> Result<String, String> {
        let sid = self.resolve_target(target)?;
        let entry = marspot_term::session_registry::list_session_entries()
            .into_iter()
            .find(|e| e.id == sid)
            .ok_or_else(|| format!("pane {sid} is gone"))?;
        let bytelog = marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog");
        marspot::pane_read::screen_text(
            &bytelog,
            entry.cols,
            entry.rows,
            extra_lines.min(u16::MAX as u32) as u16,
        )
        .map_err(|e| format!("cannot read pane {sid}: {e}"))
    }

    /// Type `text` into the pane called `target`, then Enter.
    ///
    /// The whole of "session-to-session communication" so far, and
    /// deliberately so: it is the same delivery every future protocol
    /// needs, and everything hard about it is already here — naming a
    /// pane that has no name, not colliding with whatever that pane is
    /// already doing, and putting multi-line text in front of a program
    /// without it being executed a line at a time.
    fn run_cli_send(&mut self, target: &str, text: &str) -> (bool, String) {
        let sid = match self.resolve_target(target) {
            Ok(sid) => sid,
            Err(e) => return (false, e),
        };
        if self.pty_ops.is_busy(sid) {
            // Refuse rather than queue: the caller is a person or a
            // program that wants to know its message went in *now*, and
            // a message that lands after a reclamation finishes has
            // landed somewhere else than the sender meant.
            return (
                false,
                format!(
                    "pane {sid} is busy ({})",
                    self.pty_ops.running(sid).unwrap_or("?")
                ),
            );
        }
        let op = plugins::pty_op::PtyOp::new("cli.send")
            // Nothing is frozen and no keys are locked: this is a
            // delivery, not a takeover.  The user can keep typing in
            // that pane while it happens.
            .lock_keys(false)
            .step(plugins::pty_op::Step::paste(text))
            // Enter separately, and *after* the paste has been handed
            // over: inside the pasted text it would be part of the
            // message, and bracketed paste is exactly the mechanism
            // that stops a newline from submitting.
            .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        match self.pty_ops.submit(sid, op) {
            Some(id) => (true, format!("queued on pane {sid} (id={})", id.0)),
            None => (false, format!("pane {sid} has too much queued")),
        }
    }

    /// Hand a focus change to the pane's own session, if it has one.
    ///
    /// Mirrors `dispatch_pane_session_key` — same host, same
    /// end-on-request handling — because a restore that starts when
    /// the user looks at the pane is the difference between "it came
    /// back" and "it took forever".
    fn dispatch_pane_session_focus(&mut self, sid: u64) {
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
        active.session.on_focus(&host);
        if end_flag.get() {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
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
        // Read before `on_end` — that is where the hold is released,
        // and after it this session no longer speaks for the pane.
        let froze_picture = active.session.caps()
            & marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID
            != 0;
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
        // The freeze just lifted.  `sweep_pane_status` held this pane's
        // brightness for as long as its picture was held, so the pane
        // is still wearing the level it was parked at — and the sweep
        // that would notice is up to a second away.  A second is long
        // enough to watch the content come back and the pane light up
        // afterwards as two separate events.
        //
        // Sweeping *here* rather than sending a level from the last
        // snapshot: that snapshot describes the parked pane (the
        // machines kept running while only the presentation was
        // frozen), so it would dim the pane at the exact moment its
        // program came back.  Stepping the machines first is what makes
        // the level true.
        if froze_picture {
            self.pane_status.force_next_sweep();
            self.sweep_pane_status();
        }
    }

}

/// Fold this process's live window frames into the list already on
/// disk: index *i* takes the live frame when there is one, otherwise
/// keeps whatever was saved there, and entries past the live windows
/// are preserved.
///
/// Both halves of that matter, and both were bugs:
///   - a live window with no frame yet (`None`) must NOT be skipped —
///     skipping shifts every later window's entry by one;
///   - the on-disk tail must NOT be truncated — during a cold-launch
///     restore those entries are the geometry the core is about to ask
///     for, and they are gone by the time it does.
fn merge_window_frames(
    prev: &[marspot::state::SavedWindow],
    live: &[Option<marspot::state::SavedWindow>],
) -> Vec<marspot::state::SavedWindow> {
    let mut out: Vec<marspot::state::SavedWindow> =
        Vec::with_capacity(prev.len().max(live.len()));
    for i in 0..prev.len().max(live.len()) {
        match live.get(i).and_then(|v| v.clone()) {
            Some(fresh) => out.push(fresh),
            // No live frame for this slot: keep the saved one.  With
            // nothing saved either (a window that somehow hasn't
            // reported its frame and has no history), write a zeroed
            // hole rather than skipping the slot — a skip would shift
            // every later entry, while a hole is rejected by the
            // restore path's own `w > 50 && h > 50` check and just
            // means "this one opens at the default rect".
            None => out.push(
                prev.get(i).cloned().unwrap_or(marspot::state::SavedWindow {
                    display_id: 0,
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                }),
            ),
        }
    }
    out
}

/// Lay the open windows out by their own slot, ready to be merged
/// over what is already on disk.
///
/// Indexed by `frame_index`, not by position: a window that closed
/// leaves a gap here, and the gap is what keeps the windows that are
/// still open pointing at their own geometry.  Writing by position
/// meant closing the first of two windows moved the second into slot
/// 0 — so the two swapped frames on the next launch.
fn live_frames_by_slot(
    windows: impl Iterator<Item = (usize, Option<marspot::state::SavedWindow>)>,
    saved_len: usize,
) -> Vec<Option<marspot::state::SavedWindow>> {
    let mut out: Vec<Option<marspot::state::SavedWindow>> = vec![None; saved_len];
    for (slot, frame) in windows {
        if out.len() <= slot {
            out.resize(slot + 1, None);
        }
        out[slot] = frame;
    }
    out
}

#[cfg(test)]
mod window_frame_merge_tests {
    use super::{live_frames_by_slot, merge_window_frames};
    use marspot::state::SavedWindow;

    fn win(x: f64) -> SavedWindow {
        SavedWindow { display_id: 1, x, y: 10.0, w: 1200.0, h: 800.0 }
    }

    /// The 2026-08-03 half of the same bug: close the FIRST of two
    /// windows and the survivor used to write itself into slot 0,
    /// overwriting the closed window's geometry — so when the closed
    /// window came back (it is parked, not discarded) the two had
    /// swapped places.
    #[test]
    fn a_closed_window_keeps_its_slot_for_the_window_still_open() {
        let prev = vec![win(0.0), win(872.0)];
        // Window 0 closed; window 1 is still open and still slot 1.
        let live = live_frames_by_slot(
            [(1usize, Some(win(880.0)))].into_iter(),
            prev.len(),
        );
        let out = merge_window_frames(&prev, &live);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].x, 0.0, "the closed window's geometry is untouched");
        assert_eq!(out[1].x, 880.0, "the open one wrote its own slot");
    }

    /// A window opened past everything on disk grows the list.
    #[test]
    fn a_window_in_a_new_slot_extends_the_list() {
        let live = live_frames_by_slot(
            [(0usize, Some(win(1.0))), (2usize, Some(win(2.0)))].into_iter(),
            1,
        );
        assert_eq!(live.len(), 3);
        assert!(live[1].is_none(), "the gap stays a gap");
        assert_eq!(live[2].as_ref().unwrap().x, 2.0);
    }

    /// The silent-update regression: one live window, two on disk.
    /// The second entry is the geometry the boot restore is about to
    /// ask for, so it has to survive window 0's save.
    #[test]
    fn a_single_live_window_does_not_delete_the_other_saved_frames() {
        let prev = vec![win(0.0), win(872.0)];
        let live = vec![Some(win(5.0))];
        let out = merge_window_frames(&prev, &live);
        assert_eq!(out.len(), 2, "saved tail must survive");
        assert_eq!(out[0].x, 5.0, "live window wins its own slot");
        assert_eq!(out[1].x, 872.0, "window 1 keeps its saved geometry");
    }

    /// A live window with no frame yet keeps its slot pointing at the
    /// saved value — the old `filter_map` dropped it and shifted the
    /// windows after it up by one.
    #[test]
    fn a_frameless_live_window_keeps_its_slot_instead_of_shifting() {
        let prev = vec![win(0.0), win(100.0), win(200.0)];
        let live = vec![Some(win(1.0)), None, Some(win(201.0))];
        let out = merge_window_frames(&prev, &live);
        assert_eq!(
            out.iter().map(|w| w.x).collect::<Vec<_>>(),
            vec![1.0, 100.0, 201.0],
        );
    }

    /// Nothing live and nothing saved for a slot: a hole, not a shift.
    #[test]
    fn a_slot_with_no_history_becomes_a_hole_not_a_shift() {
        let live = vec![None, Some(win(9.0))];
        let out = merge_window_frames(&[], &live);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].w, 0.0, "hole is zero-sized (restore rejects it)");
        assert_eq!(out[1].x, 9.0, "later window keeps index 1");
    }

    #[test]
    fn a_new_window_past_the_saved_list_is_appended() {
        let prev = vec![win(0.0)];
        let live = vec![Some(win(0.0)), Some(win(500.0))];
        let out = merge_window_frames(&prev, &live);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].x, 500.0);
    }

    #[test]
    fn no_windows_and_no_history_writes_nothing() {
        assert!(merge_window_frames(&[], &[]).is_empty());
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
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        let (x, y, w, h) = ctx.window_frame_pt();
        let display_id = ctx.window_display_id().unwrap_or(0);
        let cur = (x.round(), y.round(), w.round(), h.round(), display_id);
        if self.windows[wi].last_saved_window == Some(cur) {
            return;
        }
        self.windows[wi].last_saved_window = Some(cur);
        self.save_window_frames();
    }

    /// RFC-005 step 6 — write every window's frame, in creation order,
    /// so entry *i* keeps pairing with window *i* of `shell-state.bin`.
    ///
    /// Merged against the previous list rather than replacing it.  The
    /// replacing version broke every silent update: an L1 self-execv
    /// tears down all the NSWindows, the successor opens window 0 (at
    /// the frame handed over in `MARSPOT_RESTORE_FRAME`) and saves — at
    /// which point `self.windows.len() == 1`, so the file was rewritten
    /// with one entry and window 1's geometry was deleted.  Moments
    /// later the core asked for window 1 to be reopened, the restore
    /// found no entry for index 1, and the window came back at the
    /// default 1200×800 rect.  `shell.window.restore_no_frame
    /// frame_index=1` was in the log for every single update.
    ///
    /// So: overwrite by index, never shorten.  A window that closed
    /// leaves its last geometry behind, which is harmless — the core
    /// drives restores from its own per-window layout records, and
    /// nothing asks for an index it has no window for.
    fn save_window_frames(&mut self) {
        let live = live_frames_by_slot(
            self.windows.iter().map(|w| {
                (
                    w.frame_index,
                    w.last_saved_window.map(|(x, y, ww, h, display_id)| {
                        marspot::state::SavedWindow { display_id, x, y, w: ww, h }
                    }),
                )
            }),
            self.saved_window_frames.len(),
        );
        let frames = merge_window_frames(&self.saved_window_frames, &live);
        if frames.is_empty() {
            return;
        }
        if let Err(e) = marspot::state::write_windows(&frames) {
            marspot::lx_warn!(
                "shell.window_state.write_failed",
                &format!("{e}")
            );
            return;
        }
        self.saved_window_frames = frames;
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

        // Restore the dev panel's saved frame, if any.  VISIBILITY is
        // deliberately NOT restored (2026-07-18 user request): the
        // dev panel is a development tool, and persisting `visible`
        // meant one debugging session made it auto-open on every
        // subsequent launch.  Cold start = hidden, always; the
        // toolbar's 4th icon summons it, and the saved geometry still
        // puts it back where it was.  (`saved.visible` keeps being
        // WRITTEN for format stability; only the read side ignores it.)
        if let Some(saved) = marspot::state::read_dev_window() {
            marspot::dev_window::with_dev_window(|w| {
                w.apply_saved_frame(saved.x, saved.y, saved.w, saved.h);
            });
            self.dev_panel.visible = false;
            // Seed the dedup tuple so the first `dev_window_changed`
            // tick doesn't trip a save with the same values.
            self.last_saved_dev_window = Some((
                saved.x.round(), saved.y.round(),
                saved.w.round(), saved.h.round(),
                saved.display_id, false,
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
        // `resumed` fires for the boot window, which the app state
        // created before any callback could run.
        let wi = self.window_index(ctx.window_id()).unwrap_or(0);
        self.windows[wi].surfaces = Some(pair);
        self.windows[wi].presenter = Some(presenter);

        self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
        if self.active.is_some() {
            self.record_core_boot();
        }
        self.start_redraw_pump();
        ctx.request_redraw();
        // Dev-only seam: open N extra windows exactly as Cmd-N does.
        // A keystroke cannot be delivered to the sandbox app from a
        // script, and the fresh-window path (1×1 grid, one brand-new
        // session) is otherwise untestable — the restore path covers
        // everything except that.  Unset in the installed app.
        if std::env::var("MARSPOT_DEV_CLOSE_EXTRA").is_ok() {
            self.dev_close_extra = true;
        }
        if let Ok(n) = std::env::var("MARSPOT_DEV_EXTRA_WINDOWS") {
            let n: usize = n.parse().unwrap_or(0);
            for _ in 0..n.min(8) {
                self.open_new_window();
            }
            if n > 0 {
                lx_event!(
                    "DEV_EXTRA_WINDOWS",
                    "opened extra windows on request (dev seam)",
                    n = n
                );
            }
        }
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

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
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
        // Cmd-N — open another window.  Handled here rather than being
        // forwarded to the core: L1 owns windows.
        //
        // This was disabled from 2026-07-26 until RFC-005 steps 4d/6/4e
        // landed.  What made it unsafe was not the keystroke but what
        // the rest of the system did with a second window: the core
        // saved only the key window's panes, and a new window becomes
        // key the instant it appears, so sixteen panes were written out
        // of the file boot assembly reads.  The sessions survived
        // (their dirs and bytelogs are the real store) but the layout
        // did not.  `shell-state.bin` v2 saves every window, so opening
        // one can no longer narrow what gets persisted.
        if mods.super_
            && !mods.control
            && !mods.alt
            && event.state == marspot::input::KeyState::Pressed
            && matches!(
                event.logical,
                marspot::input::LogicalKey::Char(c) if c.eq_ignore_ascii_case(&'n')
            )
        {
            self.open_new_window();
            return;
        }
        let wire = event_to_wire(&event, mods);
        let w = Self::event_window(ctx);
        self.send(MsgType::KeyEvent, encode_key_event(&wire, w));
    }

    fn mouse_down(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseDown,
            encode_mouse(x, y, struct_to_mods_byte(mods), w),
        );
    }

    fn mouse_right_down(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        mods: Modifiers,
    ) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseRightDown,
            encode_mouse(x, y, struct_to_mods_byte(mods), w),
        );
    }

    fn mouse_drag(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        hover_window_id: u32,
        hover_x: f64,
        hover_y: f64,
    ) {
        // No modifier info on drag — pass zero; the renderer doesn't
        // currently need mods for drag-extend selection.
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseDrag,
            marspot::shell_proto::encode_mouse_drag(
                x, y, 0, w, hover_window_id, hover_x, hover_y,
            ),
        );
    }

    fn mouse_up(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        drop_window_id: u32,
        drop_x: f64,
        drop_y: f64,
    ) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseUp,
            marspot::shell_proto::encode_mouse_up(
                x, y, 0, w, drop_window_id, drop_x, drop_y,
            ),
        );
    }

    fn file_drop(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, paths: &[String]) {
        // L2 owns the pane layout — forward drop point + raw paths;
        // it hit-tests the pane and shell-quotes before insertion.
        let w = Self::event_window(ctx);
        self.send(MsgType::FileDrop, encode_file_drop(x, y, paths, w));
    }

    fn mouse_moved(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64) {
        // Forwarded raw — L2 hit-tests against chrome rects and
        // ignores moves that don't change its hover region (cheap).
        let w = Self::event_window(ctx);
        self.send(MsgType::MouseMove, encode_mouse(x, y, 0, w));
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise, w));
    }

    /// The OS's own controls moved (full-screen transition) while the
    /// window's pixels did not.  One small frame, no surface churn —
    /// see `MsgType::WindowChrome`.
    fn chrome_changed(&mut self, ctx: &MarspotAppCtx) {
        self.send(
            MsgType::WindowChrome,
            encode_window_chrome(
                Self::event_window(ctx),
                ctx.traffic_lights_right_phys(),
            ),
        );
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // F3+6.1 — persist window frame on every resize step.  Atomic
        // rename means a live drag can fire 60+ saves/s and the file
        // is always valid; the cost (~50us memcpy + 1 syscall) is
        // well below the per-frame resize budget.
        self.save_window_state_if_changed(ctx);
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        // Read the gate before taking `&mut` on the same window.
        let ready = self.windows[wi].first_frame_ready;
        if let Some(p) = self.windows[wi].presenter.as_mut() {
            p.set_drawable_size(w_phys, h_phys);
            // Present *synchronously* inside the resize callback so
            // our drawable lands in the SAME CATransaction AppKit is
            // about to commit for the window-bounds change.  Coupled
            // with `setPresentsWithTransaction(true)` on the layer
            // this gives Sublime-style frame-perfect resize — the
            // window edge and the drawable contents move together,
            // no inter-frame drift.
            if ready {
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
                if let Some(stale) = self.windows[wi].pending_surfaces.take() {
                    stale.release();
                }
                let (f, b) = pair.ids();
                self.windows[wi].pending_surfaces = Some(pair);
                self.send_surface_attach(
                    Self::event_window(ctx), f, b, w_phys, h_phys, scale,
                    ctx.traffic_lights_right_phys(),
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

    fn focused(&mut self, _ctx: &MarspotAppCtx, focused: bool) {
        self.send(MsgType::Focus, encode_focus(focused));
        let returning = focused && !self.app_focused;
        self.app_focused = focused;
        if returning {
            self.queue_parked_wakes();
        }
        // Silent-update trigger: the user just left marspot's window
        // (cmd-tab, click on another app, minimise).  If a pending
        // binary is staged in `binaries/pending/`, this is the
        // cheapest moment to swap — they're not watching us repaint.
        // The shell window stays put through the swap, the new core
        // attaches to the same IOSurface and shelld session, so when
        // they come back they see the same content rendered by the
        // new version's renderer.
        //
        // Skipped while a probe is in flight: one staged binary at a
        // time, and the probe already decided what happens next.
        if !focused && matches!(self.sup_state, SupervisorState::Idle) {
            if std::env::var_os("MARSPOT_MANUAL_UPDATE_ONLY").is_none() {
                self.apply_pending_update();
            }
        }
    }

    fn ime_preedit_changed(&mut self, ctx: &MarspotAppCtx, text: &str) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Preedit, encode_preedit(text, w));
    }

    fn window_opened(&mut self, ctx: &MarspotAppCtx) {
        // The window exists now, so its own metrics are readable — a
        // window opened on another display can have a different
        // backing scale, and sizing its pair off the key window would
        // hand it a mismatched surface.
        let window_id = ctx.window_id();
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let pair = match SurfacePair::create(
            w_phys.max(64.0) as usize,
            h_phys.max(64.0) as usize,
        ) {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.window.pair_create_failed", &format!("{e}"));
                marspot::app::close_window(window_id);
                return;
            }
        };
        let (f, b) = pair.ids();
        let slot = self
            .pending_frame_index
            .remove(&window_id)
            .unwrap_or_else(|| self.next_frame_index());
        let mut win = ShellWindow::new(window_id, slot);
        // The presenter is what actually puts the IOSurface on the
        // NSView — the boot window gets one in `resumed()`, and a
        // window without one is a permanently black rectangle no
        // matter how faithfully the core paints (2026-07-28 field
        // report: Cmd-N opened exactly that).  Built against the pair
        // we just created; the SurfaceReady promote re-points it at
        // whichever half the core acks.
        match ShellPresenter::new(
            ctx.ns_view(),
            scale as f32,
            &pair.front,
            &pair.back,
        ) {
            Ok(p) => {
                win.presenter = Some(p);
                lx_event!(
                    "WINDOW_PRESENTER_READY",
                    "presenter attached to the new window's view",
                    window_id = window_id
                );
            }
            Err(e) => {
                lx_error!("shell.window.presenter_new_failed", &format!("{e}"));
                pair.release();
                marspot::app::close_window(window_id);
                return;
            }
        }
        win.pending_surfaces = Some(pair);
        // Seed the geometry cache from the window as it actually
        // opened.  `window-state.bin` is a list written whole on every
        // change, so a window with no cached frame would leave a hole
        // in it and shift every later window's entry.
        let (fx, fy, fw, fh) = ctx.window_frame_pt();
        win.last_saved_window = Some((
            fx.round(),
            fy.round(),
            fw.round(),
            fh.round(),
            ctx.window_display_id().unwrap_or(0),
        ));
        if self.dev_close_extra && window_id != marspot::shell_proto::FIRST_WINDOW_ID {
            win.dev_close_after_paint = true;
        }
        self.windows.push(win);
        self.save_window_frames();
        // A `SurfaceAttachWindow` naming a window the core has not seen
        // IS that window's birth event — there is no separate create
        // frame.
        self.send_surface_attach(
            window_id, f, b, w_phys, h_phys, scale,
            ctx.traffic_lights_right_phys(),
        );
        lx_event!(
            "WINDOW_OPENED",
            "new window announced to core",
            window_id = window_id,
            windows = self.windows.len()
        );
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        // RFC-005 — closing one of several windows closes THAT window
        // and the panes it holds; only the last window closing is a
        // quit.  Cmd-Q arrives through the same callback but via the
        // NSApplicationDelegate, which the boot window's delegate
        // wears, so it always lands on the boot window and falls
        // through to the quit path below when it is the only one left.
        if marspot::app::window_count() > 1 {
            // The red button / Cmd-W: putting a window away, not
            // dismantling it.  Its slot is kept so the layout and
            // the geometry both come back on the next launch.
            self.close_window_by_id(Self::event_window(ctx), false);
            return;
        }
        self.quit(ctx);
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

        // Present EVERY window that owes a frame, not the one this
        // callback happens to be about.  All `proxy.wake()`s dispatch
        // to the boot window by design ("a wake is about the process"),
        // so a present step keyed to `ctx.window_id()` only ever ran
        // for window 1 — a second window had core frames, an installed
        // pair, even a presenter, and still showed black because
        // nothing ever called its `present()` (2026-07-28, the black
        // window's SECOND root cause).  Same class of bug as the L2
        // render pass before step 4d: a per-window duty attached to
        // one window.
        let now = Instant::now();
        for wi in 0..self.windows.len() {
            // Hold off until the core has written real content into
            // this window's pair.  Without this gate the user sees an
            // uninitialised IOSurface for ~50-100 ms at startup, then
            // a hard snap to content — a black-then-content flash.
            if !self.windows[wi].first_frame_ready {
                continue;
            }
            // Gate on a fresh frame actually pending.  See the
            // `frame_pending` field doc — the safety-net 250 ms timer
            // wakes this callback unconditionally, and sampling the
            // IOSurface on every tick races the core's mid-render
            // state (the historical all-panes flash).
            //
            // Stale safety net: if it's been a long while since this
            // window's last present (core stuck but alive — no future
            // poke coming), force one so the window doesn't freeze.
            // Threshold deliberately wide (5 s): every spurious
            // present makes WindowServer composite again, and sub-LSB
            // alpha rounding reads as a faint background pulse at 1 Hz.
            let stale = self.windows[wi]
                .last_present_at
                .map(|t| now.duration_since(t) > Duration::from_secs(5))
                .unwrap_or(true);
            if !self.windows[wi].frame_pending && !stale {
                continue;
            }
            if let Some(p) = self.windows[wi].presenter.as_mut() {
                p.present();
                self.windows[wi].frame_pending = false;
                let first = self.windows[wi].last_present_at.is_none();
                self.windows[wi].last_present_at = Some(now);
                // One INFO per window per boot: the present-side
                // mirror of the core's WINDOW_FIRST_FRAME.  The chain
                // "painted → pair installed → presenter exists" logged
                // green twice while the screen stayed black; only the
                // present itself is proof the pixels went up.
                if first {
                    lx_event!(
                        "WINDOW_FIRST_PRESENT",
                        "first present for this window",
                        window_id = self.windows[wi].window_id
                    );
                }
            }
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
                        .map(|(rect, win)| ShellInbox::CaretRect(rect, win)),
                    // Empty-payload wake: a fresh frame is in the IOSurface.
                    // Mapping it to Some(..) is what makes `proxy.wake()`
                    // fire below → user_event → present.
                    MsgType::FrameRendered => Some(ShellInbox::FrameRendered),
                    MsgType::PaneBadgeClicked => {
                        marspot::shell_proto::decode_pane_badge_clicked(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneBadgeClicked)
                    }
                    MsgType::PaneFocused => {
                        marspot::shell_proto::decode_pane_focused(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneFocused)
                    }
                    MsgType::PaneBadgeMenuRequest => {
                        marspot::shell_proto::decode_pane_badge_menu_request(&frame.payload)
                            .ok()
                            .map(|(sid, x, y)| ShellInbox::PaneBadgeMenuRequest(sid, x, y))
                    }
                    MsgType::PaneBadgeMenuAction => {
                        marspot::shell_proto::decode_pane_badge_menu_action(&frame.payload)
                            .ok()
                            .map(|(sid, tag)| ShellInbox::PaneBadgeMenuAction(sid, tag))
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
                    MsgType::WindowOpenRequest => {
                        marspot::shell_proto::decode_window_open_request_at(&frame.payload)
                            .ok()
                            .map(|(idx, hint)| ShellInbox::WindowOpenRequest(idx, hint))
                    }
                    MsgType::WindowCloseRequest => {
                        marspot::shell_proto::decode_window_close_request(&frame.payload)
                            .ok()
                            .map(ShellInbox::WindowCloseRequest)
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
    // RFC-004 D.1 — one-time Caches → Application Support state-root
    // migration.  Must run before logx / any path computation.
    marspot::paths::migrate_legacy_state_root();
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
        Some("--panes") => {
            match cli_socket::list_panes() {
                Ok(mut panes) => {
                    // Sorted by address, not by whatever order the
                    // registry happened to be in: a list you read is a
                    // list you scan.
                    panes.sort_by(|a, b| a.2.cmp(&b.2));
                    println!("{:>6}  {:<34}  {}", "id", "address", "directory");
                    for (sid, cwd, addr) in panes {
                        println!("{sid:>6}  {addr:<34}  {cwd}");
                    }
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--autorun") => {
            // `--autorun` alone lists; `--autorun <pane> [on|off]`
            // switches or asks about one.
            let target = args.get(2).cloned().unwrap_or_default();
            let on = match args.get(3).map(String::as_str) {
                Some("on") => Some(true),
                Some("off") => Some(false),
                None => None,
                Some(other) => {
                    eprintln!("unknown mode {other:?}; use on or off");
                    std::process::exit(2);
                }
            };
            match cli_socket::autorun(&target, on) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--read") => {
            // `marspot-shell --read <pane> [-n <lines>]` — the pane's
            // screen, plus that many lines of what scrolled off it.
            let target = args.get(2).cloned().unwrap_or_default();
            let extra = args
                .iter()
                .position(|a| a == "-n")
                .and_then(|i| args.get(i + 1))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            if target.is_empty() {
                eprintln!("usage: marspot-shell --read <pane> [-n <lines>]");
                std::process::exit(2);
            }
            match cli_socket::read_pane(&target, extra) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--send") => {
            // `marspot-shell --send <pane> <text…>` — the rest of argv
            // is the message, joined with spaces, so it can be written
            // without quoting in the common case.
            let target = args.get(2).cloned().unwrap_or_default();
            let text = args[3.min(args.len())..].join(" ");
            if target.is_empty() || text.is_empty() {
                eprintln!("usage: marspot-shell --send <pane> <text…>");
                std::process::exit(2);
            }
            match cli_socket::send_text(&target, &text) {
                Ok((true, msg)) => {
                    println!("{msg}");
                }
                Ok((false, msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
"marspot-shell — supervisor for the marspot terminal.\n\
\n\
Usage:\n\
  marspot-shell                Start the supervisor (window + core).\n\
  marspot-shell --version      Print version / git / build info.\n\
  marspot-shell --status       Summarise state from supervisor.log + live PIDs.\n\
  marspot-shell --panes        List panes: session id, name, working directory.\n\
  marspot-shell --autorun [<pane> [on|off]]\n\
                               Keep a rotation going by itself: when the session\n\
                               says it is done, send /clear then 继续 autorun;\n\
                               when it stalls on a server error, nudge it with\n\
                               继续, backing off each time.  No argument lists\n\
                               the panes it is on for.\n\
  marspot-shell --read <pane> [-n <lines>]\n\
                               Print what the pane says: its screen, plus that\n\
                               many lines of what has scrolled off it.\n\
  marspot-shell --send <pane> <text…>\n\
                               Type text into a pane and press Enter.  The pane\n\
                               is named by its working directory's last component\n\
                               (e.g. `spg`), a path tail (`goliajp/spg`), or a\n\
                               session id.  An ambiguous name is refused, and\n\
                               the error lists the candidates with their ids.\n\
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

    // Same reasoning as L2's loop: this thread runs the supervisor\n    // tick and every AppKit callback, and it was measured 100.8 s\n    // late during the 2026-08-11 incident.
    // Measured on an idle bench host with background load as the only
    // variable: at default QoS a frame's p99 went from 293 µs (idle)
    // to 6,082 µs (load 11), while the GPU's own account of the same
    // frame never moved (111 → 115 µs).  Nothing got heavier; this
    // thread simply stopped being scheduled.  Raising it to
    // USER_INTERACTIVE, same machine and load, seconds apart, both
    // orderings: p99 325 µs — the tail is gone.  See `marspot::qos`.
    let qos_ok = marspot::qos::raise_current_thread_to_user_interactive();
    lx_event!(
        "STARTUP",
        "marspot-shell starting",
        qos_user_interactive = qos_ok as u32,
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
    // 2026-07-28 — crash-loop brake.  Runs before anything expensive
    // (updater thread, AppKit, core spawn): if this launch is part of
    // a rapid-restart burst, sleep first so the burst cannot saturate
    // the machine, and past the safe-mode threshold flag the whole
    // boot as spawn-nothing-fresh (`MARSPOT_SAFE_MODE`, read by this
    // process AND inherited by the core it spawns).
    match assess_launch_history(&read_launch_stamps(), now_unix_f64()) {
        LaunchVerdict::Normal => {}
        LaunchVerdict::Backoff(secs) => {
            lx_warn!(
                "shell.crash_loop.backoff",
                "rapid relaunches detected — sleeping before boot",
                sleep_s = secs
            );
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
        LaunchVerdict::SafeMode(secs) => {
            lx_event!(
                "SHELL_SAFE_MODE",
                "crash loop detected — this boot reattaches but spawns nothing fresh",
                sleep_s = secs
            );
            sup_log::log("SHELL_SAFE_MODE", "crash loop — safe-mode boot");
            // SAFETY: startup path in main(), before any thread is
            // spawned; children inherit it.
            unsafe { std::env::set_var("MARSPOT_SAFE_MODE", "1") };
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }
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
        // SAFETY: startup path in main(), before any thread is spawned.
        unsafe { std::env::remove_var("MARSPOT_RESTORE_FRAME") };
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
        // The boot window is entry 0 of the list; the rest are applied
        // as their windows are reopened.
        let w = marspot::state::read_windows()?.into_iter().next()?;
        if w.w > 50.0 && w.h > 50.0 { Some((w.x, w.y, w.w, w.h)) } else { None }
    });
    // With nothing to restore this is a first run — or the launch
    // after the user closed the last pane of the last window, which
    // deletes both saved files.  A modest window centred on the main
    // screen is the right thing to hand someone with no layout of
    // their own yet; `run_app` centres it because there is no frame.
    let (boot_w, boot_h) = match restore_frame {
        Some(_) => (DEFAULT_W_PT, DEFAULT_H_PT),
        None => (FRESH_W_PT, FRESH_H_PT),
    };
    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: boot_w,
        height_logical: boot_h,
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
