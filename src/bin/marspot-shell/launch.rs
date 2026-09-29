//! The binary's front door: what happens before there is a window.
//!
//! A bundle launch redirects into `binaries/current/`, and that hand-off
//! is where a bad image can take the app down with it — so the journal,
//! the crash-loop verdict and the safe-mode backoff live here, next to
//! the redirect they guard.  The `--version` / `--status` / `--trigger`
//! / `--rollback` subcommands are here too: they answer and exit
//! without ever reaching the event loop.

use std::sync::atomic::{AtomicBool, Ordering};

use marspot::lx_error;

use crate::{sup_log, supervisor};

/// Set by the SIGUSR1 handler.  The main-thread `poll_supervisor`
/// drains it and triggers `apply_pending_update`.  Atomic + flag
/// pattern is the only safe way to interact with the main thread
/// from a signal context (no NSApp / Metal / mutex calls allowed
/// inside `sigusr1_handler`).
pub(crate) static SIGUSR1_FLAG: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn sigusr1_handler(_signum: libc::c_int) {
    SIGUSR1_FLAG.store(true, Ordering::Release);
}

/// Launch journal for shell crash-loop detection: one
/// `ts \t current_mtime` TSV row per bundle-binary launch that is
/// about to redirect into `binaries/current/marspot-shell`.  Lives
/// next to the binaries tree so a `rm -rf Caches/marspot` resets
/// both together.
pub(crate) fn shell_launch_log_path() -> std::path::PathBuf {
    marspot::paths::shell_launch_journal()
}

/// ≥ this many launches of the same `current/` binary …
pub(crate) const LAUNCH_LOOP_THRESHOLD: usize = 3;
/// … within this window ⇒ crash loop (the redirected shell is dying
/// before the user can even interact with it).
pub(crate) const LAUNCH_LOOP_WINDOW_SECS: f64 = 60.0;
/// Journal stays bounded: once it crosses this many lines we rewrite
/// it down to the trailing half.  One row per launch, so this is
/// generous.
pub(crate) const LAUNCH_LOG_MAX_LINES: usize = 64;

/// Append this launch to the journal, then report whether the last
/// `LAUNCH_LOOP_THRESHOLD` rows (including this one) all point at the
/// same `current/` binary (by mtime) inside `LAUNCH_LOOP_WINDOW_SECS`.
/// That signature means the binary we keep redirecting into never
/// lives long enough to matter — a broken self-update would otherwise
/// wedge the app in an exec → crash → relaunch loop forever.
pub(crate) fn record_launch_and_detect_loop(current: &std::path::Path) -> bool {
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
pub(crate) enum LaunchVerdict {
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

pub(crate) const RAPID_WINDOW_SECS: f64 = 60.0;
pub(crate) const RAPID_THRESHOLD: usize = 3;
pub(crate) const SAFE_MODE_WINDOW_SECS: f64 = 300.0;
pub(crate) const SAFE_MODE_THRESHOLD: usize = 5;
pub(crate) const BACKOFF_CAP_SECS: u64 = 30;

/// Pure so it can be pinned by tests: `stamps` are the journal's
/// launch timestamps (any order), `now` is this launch's clock.  The
/// journal row for this launch is already appended by the time the
/// redirected shell runs, so the counts include self.
pub(crate) fn assess_launch_history(stamps: &[f64], now: f64) -> LaunchVerdict {
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

pub(crate) fn now_unix_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Read the journal's timestamps.  Bad rows just don't count.
pub(crate) fn read_launch_stamps() -> Vec<f64> {
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
pub(crate) fn maybe_redirect_to_current_shell() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("MARSPOT_NO_REDIRECT").is_some() {
        return;
    }
    // Redirecting costs the whole app its Gatekeeper exemption.
    //
    // `exec` replaces the image, and with it the process's RESPONSIBLE
    // identity: it stops being `Marspot.app` and becomes a bare path
    // under Application Support.  A bare path cannot hold a bundle-id
    // TCC grant, so the Developer Tools exemption — the thing that
    // lets a terminal run code its user just compiled without a
    // Gatekeeper scan — no longer matches.  Measured on this machine,
    // same script, same minute:
    //
    //   responsible = Marspot.app        0.00 s, performScan 0
    //   responsible = current/ bare path 0.30 s, performScan every time
    //
    // Under load the second row is 1.3 s, and on a busy test tier it
    // has been reported at tens of seconds.  Every binary the user
    // builds inside marspot pays it once.  `Terminal.app` and
    // `iTerm.app` do not, and this is the only reason.
    //
    // So the redirect is off by default.  The cost is that a NEW L1
    // lands on the next cold launch instead of instantly: the bundle
    // binary cannot be overwritten while its own process is running
    // (AMFI kills the process when the on-disk CDHash stops matching
    // — 2026-06-16, nine panes went blank that way).  L1 moves rarely
    // (0.7.x against core's 0.12.x), and L2/L3 keep updating live:
    // they are forked children, and a forked child inherits the
    // responsible process, so they can live outside the bundle
    // without costing anything.  `MARSPOT_REDIRECT=1` restores the
    // old behaviour for a bisect.
    if std::env::var_os("MARSPOT_REDIRECT").is_none() {
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
pub(crate) fn cmd_rollback(which: &str) -> i32 {
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

pub(crate) fn print_version() {
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
pub(crate) fn find_running_shell_pid() -> Option<u32> {
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

pub(crate) fn print_status() {
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

pub(crate) fn sup_log_path() -> std::path::PathBuf {
    marspot::paths::supervisor_log()
}

/// `--trigger` — send SIGUSR1 to the running shell so it applies any
/// staged pending update.  Returns the process exit code:
///   0 = signal sent successfully
///   1 = no running shell found
///   2 = signal call errored
/// Record this GUI shell's pid under the state dir.  Best-effort —
/// `--trigger` falls back to a `ps` scan if the file is missing.
pub(crate) fn write_shell_pid() {
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
pub(crate) fn running_shell_pid() -> Option<u32> {
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

pub(crate) fn cmd_trigger() -> i32 {
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

pub(crate) fn install_sigusr1_handler() {
    // SAFETY: registering a handler is signal-safe; the handler we
    // register only touches an AtomicBool.
    unsafe {
        libc::signal(
            libc::SIGUSR1,
            sigusr1_handler as *const () as libc::sighandler_t,
        );
    }
}
