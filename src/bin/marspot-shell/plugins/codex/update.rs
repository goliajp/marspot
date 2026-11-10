//! Keeping codex panes on the newest codex.
//!
//! codex installs two packages under `$CODEX_HOME/packages`: the
//! app-server daemon, which updates itself hourly, and the CLI the panes
//! run, which does not.  Left alone the two drift apart for days.  The
//! follower installs the CLI once the daemon has moved past it; the
//! refresher moves a pane whose TUI is older than the installed CLI onto
//! it, on the same thread.  Neither touches the daemon: it holds every
//! thread and every tool run, so taking a TUI down loses no work.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marspot::pidtree::ProcId;
use marspot_term::{lx_info, lx_warn};

/// A codex release, `major.minor.patch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Release(u32, u32, u32);

impl Release {
    /// `0.164.0`, or a release directory, `0.164.0-aarch64-apple-darwin`.
    pub(crate) fn parse(s: &str) -> Option<Release> {
        let v = s.trim().split('-').next()?;
        let mut it = v.split('.').map(|p| p.parse::<u32>().ok());
        let r = Release(it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(r)
    }

    /// The release an executable belongs to, from where it sits in a
    /// package: `…/releases/<release>/bin/codex`.  None for a codex that
    /// is not in one (a cargo build, npm, brew); such a pane is never
    /// called stale, because there is nothing here that would update it.
    pub(crate) fn of_exe(path: &Path) -> Option<Release> {
        let parts: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect();
        let i = parts.iter().rposition(|p| *p == "releases")?;
        Release::parse(parts.get(i + 1)?)
    }
}

impl std::fmt::Display for Release {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The release a package has selected: the target of its `current` link.
fn selected(packages: &Path, which: &str) -> Option<Release> {
    let target = std::fs::read_link(packages.join(which).join("current")).ok()?;
    Release::parse(target.file_name()?.to_str()?)
}

pub(crate) fn installed_cli(home: &Path) -> Option<Release> {
    selected(&home.join("packages"), "standalone")
}

fn daemon_selected(home: &Path) -> Option<Release> {
    selected(&home.join("packages"), "app-server-daemon")
}

/// The command codex itself prints when `codex update` cannot tell how
/// it was installed, which is the case on a machine whose packages
/// directory is shared between profiles.
const INSTALLER: &str = "https://chatgpt.com/codex/install.sh";
/// How often the two packages are compared: two link reads.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// After a failed install, how long before the same release is tried
/// again.  The daemon updater's own cadence.
const RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Installs the CLI when the daemon has moved past it.
#[derive(Default)]
pub(crate) struct Follower {
    next_check: Option<Instant>,
    running: Option<std::thread::JoinHandle<()>>,
    /// The daemon release last installed for, and when.
    tried: Option<(Release, Instant)>,
}

impl Follower {
    pub(crate) fn tick(&mut self, home: &Path) {
        let now = Instant::now();
        if self.next_check.is_some_and(|t| now < t) {
            return;
        }
        self.next_check = Some(now + CHECK_EVERY);
        if !marspot::settings::get().codex_follow_updates {
            return;
        }
        if self.running.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        if let Some(h) = self.running.take() {
            let _ = h.join();
        }
        let (Some(daemon), Some(cli)) = (daemon_selected(home), installed_cli(home)) else {
            return;
        };
        if daemon <= cli {
            return;
        }
        if self.tried.is_some_and(|(r, at)| r == daemon && now.duration_since(at) < RETRY_AFTER) {
            return;
        }
        self.tried = Some((daemon, now));
        lx_info!("codex.update.install", "the cli is behind the daemon; installing", cli = cli.to_string(), daemon = daemon.to_string());
        let home = home.to_path_buf();
        let spawned = std::thread::Builder::new().name("codex-install".into()).spawn(move || {
            match install(&home) {
                Ok(()) => lx_info!(
                    "codex.update.installed",
                    "installer finished",
                    cli = installed_cli(&home).map(|r| r.to_string()).unwrap_or_default()
                ),
                Err(e) => lx_warn!("codex.update.install_failed", &e),
            }
        });
        match spawned {
            Ok(h) => self.running = Some(h),
            Err(e) => lx_warn!("codex.update.install_failed", &format!("spawn: {e}")),
        }
    }
}

/// Fetch codex's installer and run it the way codex's own fallback
/// tells a person to, without a terminal to ask anything on.
///
/// The `latest` channel, not a pinned release: an installer given a
/// release removes the CLI's `auto-update-version`, and codex then
/// reports the CLI as pinned.  The daemon updater follows the same
/// channel, so the two land on the same release.
pub(crate) fn install(home: &Path) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("marspot-codex-install-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let script = dir.join("install.sh");
    let r = (|| {
        let fetched = Command::new("/usr/bin/curl")
            .args(["-fsSL", "--max-time", "120", "-o"])
            .arg(&script)
            .arg(INSTALLER)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if !fetched.status.success() {
            return Err(format!("curl {INSTALLER}: {} {}", fetched.status, tail(&fetched.stderr)));
        }
        let ran = Command::new("/bin/sh")
            .arg(&script)
            .env("CODEX_NON_INTERACTIVE", "1")
            .env("CODEX_HOME", home)
            // where it installs the command; with it on PATH the
            // installer has no reason to edit a shell profile
            .env("PATH", path_with_local_bin())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("sh: {e}"))?;
        if !ran.status.success() {
            return Err(format!("installer: {} {}", ran.status, tail(&ran.stderr)));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// This process's PATH with `~/.local/bin` on it, for an installer.
pub(crate) fn path_with_local_bin() -> String {
    let path = std::env::var("PATH").unwrap_or_else(|_| marspot_term::paths::DEFAULT_PATH.into());
    match std::env::var_os("HOME") {
        Some(h) => marspot_term::paths::path_with_local_bin(&path, Path::new(&h)),
        None => path,
    }
}

pub(crate) fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().rev().take(3).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// How long a pane's terminal must have been silent before its TUI is
/// restarted.  Typing echoes, a running turn streams: a minute of
/// nothing is a minute nobody is using it.
pub(crate) const QUIET_FOR: Duration = Duration::from_secs(60);

/// What is known about one stale pane when deciding whether to restart
/// its TUI now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Evidence {
    /// This pane was already restarted toward the installed release; a
    /// second try would loop if the shell's `codex` is some other one.
    pub tried: bool,
    pub thread_known: bool,
    /// Started as `codex` or `codex resume <thread>`: a restart puts
    /// back nothing else, so other options would be dropped.
    pub plain_argv: bool,
    /// None until marspot has been told which pane has the cursor.
    pub user_here: Option<bool>,
    pub thread_idle: bool,
    pub quiet_for: Duration,
}

/// Why a stale pane is left alone, or None to restart it.  Ordered so
/// the reason logged is the one that would still hold if the later ones
/// cleared.
pub(crate) fn hold_reason(e: &Evidence) -> Option<&'static str> {
    if e.tried {
        Some("tried_once")
    } else if !e.thread_known {
        Some("thread_unknown")
    } else if !e.plain_argv {
        Some("custom_argv")
    } else if e.user_here != Some(false) {
        Some(if e.user_here.is_none() { "focus_unknown" } else { "user_here" })
    } else if !e.thread_idle {
        Some("thread_busy")
    } else if e.quiet_for < QUIET_FOR {
        Some("terminal_active")
    } else {
        None
    }
}

/// `codex`, `codex resume <id>` or `codex resume --last`, by any path
/// to a binary called `codex`.
pub(crate) fn plain_argv(argv: &[String]) -> bool {
    let named_codex = argv.first().is_some_and(|a| a.rsplit('/').next() == Some("codex"));
    named_codex
        && match &argv[1..] {
            [] => true,
            [r, _] => r == "resume",
            _ => false,
        }
}

/// Per-pane state of the refresher.
#[derive(Default)]
pub(crate) struct Refresher {
    /// The release each TUI process runs; an executable does not change
    /// under a running process, so it is read once.
    exe: HashMap<ProcId, Option<Release>>,
    /// Pane → the installed release it was last restarted toward.
    pub(crate) tried: HashMap<u64, Release>,
    /// Last hold reason logged per pane, so each is logged once.
    pub(crate) held: HashMap<u64, &'static str>,
    /// A restart is running; one at a time across every pane.
    pub(crate) busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Refresher {
    pub(crate) fn release_of(&mut self, codex: ProcId) -> Option<Release> {
        *self
            .exe
            .entry(codex)
            .or_insert_with(|| marspot_term::platform::pid_exe_path(codex.pid).as_deref().and_then(Release::of_exe))
    }

    /// Forget what belongs to panes and processes that are gone.
    pub(crate) fn prune(&mut self, live: &HashMap<u64, ProcId>) {
        let procs: std::collections::HashSet<ProcId> = live.values().copied().collect();
        self.exe.retain(|p, _| procs.contains(p));
        self.tried.retain(|sid, _| live.contains_key(sid));
        self.held.retain(|sid, _| live.contains_key(sid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_parse_from_versions_and_directories() {
        assert_eq!(Release::parse("0.164.0"), Some(Release(0, 164, 0)));
        assert_eq!(Release::parse("0.164.0-aarch64-apple-darwin\n"), Some(Release(0, 164, 0)));
        assert_eq!(Release::parse("0.164"), None);
        assert_eq!(Release::parse("0.164.0.1"), None);
        assert_eq!(Release::parse("latest"), None);
    }

    #[test]
    fn releases_order_by_number_not_by_text() {
        assert!(Release(0, 164, 0) > Release(0, 163, 0));
        assert!(Release(0, 159, 10) > Release(0, 159, 3));
        assert!(Release(1, 0, 0) > Release(0, 999, 9));
    }

    #[test]
    fn an_executable_names_its_release_only_inside_a_package() {
        let p = Path::new("/Users/x/.codex-shared/packages/standalone/releases/0.163.0-aarch64-apple-darwin/bin/codex");
        assert_eq!(Release::of_exe(p), Some(Release(0, 163, 0)));
        assert_eq!(Release::of_exe(Path::new("/Users/x/work/codex/target/release/codex")), None);
        assert_eq!(Release::of_exe(Path::new("/opt/homebrew/bin/codex")), None);
    }

    #[test]
    fn the_package_selection_is_read_off_its_link() {
        let home = std::env::temp_dir().join(format!("marspot-codex-sel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for (which, rel) in [("standalone", "0.163.0-aarch64-apple-darwin"), ("app-server-daemon", "0.164.0-aarch64-apple-darwin")] {
            let root = home.join("packages").join(which);
            std::fs::create_dir_all(root.join("releases").join(rel)).unwrap();
            std::os::unix::fs::symlink(root.join("releases").join(rel), root.join("current")).unwrap();
        }
        assert_eq!(installed_cli(&home), Some(Release(0, 163, 0)));
        assert_eq!(daemon_selected(&home), Some(Release(0, 164, 0)));
        assert_eq!(installed_cli(&home.join("nowhere")), None);
        std::fs::remove_dir_all(&home).unwrap();
    }

    fn ready() -> Evidence {
        Evidence {
            tried: false,
            thread_known: true,
            plain_argv: true,
            user_here: Some(false),
            thread_idle: true,
            quiet_for: QUIET_FOR,
        }
    }

    #[test]
    fn a_ready_pane_goes_and_each_gate_alone_holds_it_by_name() {
        assert_eq!(hold_reason(&ready()), None);
        let cases: [(Evidence, &str); 7] = [
            (Evidence { tried: true, ..ready() }, "tried_once"),
            (Evidence { thread_known: false, ..ready() }, "thread_unknown"),
            (Evidence { plain_argv: false, ..ready() }, "custom_argv"),
            (Evidence { user_here: Some(true), ..ready() }, "user_here"),
            (Evidence { user_here: None, ..ready() }, "focus_unknown"),
            (Evidence { thread_idle: false, ..ready() }, "thread_busy"),
            (Evidence { quiet_for: QUIET_FOR - Duration::from_secs(1), ..ready() }, "terminal_active"),
        ];
        for (e, why) in cases {
            assert_eq!(hold_reason(&e), Some(why), "{e:?}");
        }
    }

    #[test]
    fn only_a_plain_command_line_is_restarted() {
        let v = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(plain_argv(&v("codex")));
        assert!(plain_argv(&v("/Users/x/.local/bin/codex")));
        assert!(plain_argv(&v("codex resume 01a133f4-8e7b-7331-9630-9c40ff4b467f")));
        assert!(plain_argv(&v("codex resume --last")));
        assert!(!plain_argv(&v("codex --yolo")));
        assert!(!plain_argv(&v("codex -c model=o3 resume --last")));
        assert!(!plain_argv(&v("codex fix the build")));
        assert!(!plain_argv(&v("codex-code-mode-host")));
        assert!(!plain_argv(&[]));
    }
}
