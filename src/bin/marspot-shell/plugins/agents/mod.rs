//! The agent CLIs a pane is expected to have, installed when missing.
//!
//! A machine that has only marspot on it has no `claude`, `codex`,
//! `gemini` or `kimi`, and nothing a vendor's installer could lean on:
//! no Node, no package manager.  Each one is fetched the way its vendor
//! ships a standalone build, into `~/.local/bin` (kimi's into
//! `~/.kimi-code/bin`), which every pane has on its PATH.  A CLI that
//! is already there, however it got there, is left alone.
//!
//! Staying current is each CLI's own business where it has a way:
//! claude's native build and kimi update themselves, codex is followed
//! by the codex plugin.  gemini has none, so the one installed here is
//! replaced when a newer release is out.
//!
//! Everything runs on one background thread, a few times a day.

pub(crate) mod changes;
mod gemini;
mod kimi;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marspot_term::{lx_info, lx_warn};

use crate::plugins::codex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Cli {
    Claude,
    Codex,
    Gemini,
    Kimi,
}

impl Cli {
    const ALL: [Cli; 4] = [Cli::Claude, Cli::Codex, Cli::Gemini, Cli::Kimi];

    fn command(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Gemini => "gemini",
            Cli::Kimi => "kimi",
        }
    }
}

/// The installer claude's own documentation names.  It puts the native
/// build in `~/.local/bin/claude`, and that build updates itself.
const CLAUDE_INSTALLER: &str = "https://claude.ai/install.sh";
/// After start, so the first look does not compete with the panes
/// coming up.
const FIRST_AFTER: Duration = Duration::from_secs(15);
const EVERY: Duration = Duration::from_secs(3600);
/// After a failed install, how long before the same CLI is tried
/// again.  An hour: the usual failure is a machine that was offline
/// when marspot started, and it should not wait a day for its CLIs.
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// A login shell that has not said where it looks for commands by now
/// is not going to.
const SHELL_WITHIN: Duration = Duration::from_secs(15);
/// A newly signed binary's first run is checked by the system before
/// it starts, which has taken most of a minute.
const VERSION_WITHIN: Duration = Duration::from_secs(120);

/// Start the thread.  Not in a sandbox: it shares the developer's HOME
/// and the network.
pub(crate) fn start() {
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return;
    }
    let spawned = std::thread::Builder::new().name("agents-install".into()).spawn(|| {
        std::thread::sleep(FIRST_AFTER);
        let mut failed = Failures::default();
        loop {
            if marspot::settings::get().agents_install {
                pass(&mut failed);
            }
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        lx_warn!("agents.spawn_failed", &e.to_string());
    }
}

/// Where the CLIs are looked for and installed.
pub(crate) struct Places {
    pub home: PathBuf,
    pub path: OsString,
    pub codex_home: PathBuf,
    /// marspot's state root.
    pub state: PathBuf,
}

impl Places {
    fn of_this_process() -> Option<Places> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(Places {
            path: login_path().unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default()),
            codex_home: std::env::var_os("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from),
            state: marspot_term::paths::state_root(),
            home,
        })
    }
}

/// The PATH the user's own shell ends up with.
///
/// An app opened from Finder has the system's bare PATH, which knows
/// nothing of Homebrew, npm or anything a shell profile adds: a CLI
/// the user installed there would read as missing and be installed a
/// second time.  The shell is asked the way a pane starts it, login
/// and interactive, and answers between markers because a profile may
/// print whatever it likes.
fn login_path() -> Option<OsString> {
    const MARK: &str = "__marspot_path__";
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SHELL_WITHIN;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    path_between_marks(&out, MARK).map(OsString::from)
}

fn path_between_marks(out: &str, mark: &str) -> Option<String> {
    let rest = &out[out.find(mark)? + mark.len()..];
    let path = &rest[..rest.find(mark)?];
    (!path.is_empty()).then(|| path.to_string())
}

fn on_path(command: &str, path: &OsStr) -> bool {
    std::env::split_paths(path).any(|d| d.join(command).is_file())
}

/// A CLI counts as there when its command resolves, or sits where the
/// installers put it, which marspot's own PATH need not include.
pub(crate) fn present(cli: Cli, at: &Places) -> bool {
    on_path(cli.command(), &at.path)
        || at.home.join(".local/bin").join(cli.command()).is_file()
        || (cli == Cli::Codex && codex::installed_cli(&at.codex_home).is_some())
        || (cli == Cli::Kimi && kimi::bin(&at.home).is_file())
}

pub(crate) fn missing(at: &Places) -> Vec<Cli> {
    Cli::ALL.into_iter().filter(|c| !present(*c, at)).collect()
}

/// When each CLI's install last failed.
#[derive(Default)]
pub(crate) struct Failures(HashMap<Cli, Instant>);

impl Failures {
    pub(crate) fn due(&self, cli: Cli, now: Instant) -> bool {
        self.0.get(&cli).is_none_or(|at| now.duration_since(*at) >= RETRY_AFTER)
    }

    pub(crate) fn record(&mut self, cli: Cli, now: Instant, ok: bool) {
        if ok {
            self.0.remove(&cli);
        } else {
            self.0.insert(cli, now);
        }
    }
}

fn pass(failed: &mut Failures) {
    let Some(at) = Places::of_this_process() else { return };
    let gone = missing(&at);
    for cli in Cli::ALL {
        let now = Instant::now();
        if !failed.due(cli, now) {
            continue;
        }
        let absent = gone.contains(&cli);
        let result = match cli {
            Cli::Claude if absent => install_claude(&at.home),
            Cli::Codex if absent => codex::install_cli(&at.codex_home)
                .map(|()| codex::installed_cli(&at.codex_home).map(|r| r.to_string()).unwrap_or_default()),
            Cli::Gemini => match gemini::step(&at, !absent) {
                Ok(Some(version)) => Ok(version),
                Ok(None) => continue,
                Err(e) => Err(e),
            },
            Cli::Kimi if absent => kimi::install(&at.home),
            _ => continue,
        };
        finished(cli, &result);
        failed.record(cli, now, result.is_ok());
    }
}

/// An install has ended.  One that worked left a config directory or a
/// command that the hooks, the peer servers and the skill go into, and
/// the supervisor may be at rest: say so now rather than leave it to a
/// tick.
fn finished(cli: Cli, result: &Result<String, String>) {
    match result {
        Ok(version) => {
            lx_info!("agents.installed", "installed", cli = cli.command(), version = version);
            changes::note();
        }
        Err(e) => lx_warn!("agents.install_failed", e, cli = cli.command()),
    }
}

fn install_claude(home: &Path) -> Result<String, String> {
    let dir = scratch("claude")?;
    let script = dir.join("install.sh");
    let r = fetch(CLAUDE_INSTALLER, &script).and_then(|()| {
        // with where it installs on PATH the installer has no reason
        // to edit a shell profile
        run(Command::new("/bin/bash").arg(&script).env("PATH", codex::path_with_local_bin()), "installer")?;
        version(&home.join(".local/bin/claude"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// A new directory only this user can read, for what is downloaded
/// before it is run.
fn scratch(what: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("marspot-{what}-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn fetch(url: &str, to: &Path) -> Result<(), String> {
    run(Command::new("/usr/bin/curl").args(["-fsSL", "--max-time", "600", "-o"]).arg(to).arg(url), "curl").map(drop)
}

/// Run to the end with no terminal to ask anything on; its output when
/// it succeeds.
fn run(cmd: &mut Command, what: &str) -> Result<Vec<u8>, String> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{what}: {} {}", out.status, codex::tail(&out.stderr)));
    }
    Ok(out.stdout)
}

/// What `<bin> --version` prints, which doubles as the proof that the
/// binary runs at all.
fn version(bin: &Path) -> Result<String, String> {
    let name = bin.display();
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let deadline = Instant::now() + VERSION_WITHIN;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} --version did not finish"));
            }
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    if !status.success() {
        return Err(format!("{name} --version: {status}"));
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    Ok(out.lines().next().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(tag: &str) -> Places {
        let root = std::env::temp_dir().join(format!("marspot-agents-{tag}-{}", marspot_peer::rand::hex(6)));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        Places { path: root.join("bin").into_os_string(), codex_home: home.join(".codex"), state: root.join("state"), home }
    }

    fn cleanup(at: &Places) {
        std::fs::remove_dir_all(at.home.parent().unwrap()).unwrap();
    }

    #[test]
    fn agents_on_an_empty_machine_are_all_missing() {
        let at = places("empty");
        assert_eq!(missing(&at), Cli::ALL);
        cleanup(&at);
    }

    #[test]
    fn agents_found_on_path_or_in_local_bin_are_not_missing() {
        let at = places("found");
        std::fs::write(at.home.join(".local/bin/claude"), "").unwrap();
        assert_eq!(missing(&at), [Cli::Codex, Cli::Gemini, Cli::Kimi]);
        std::fs::write(Path::new(&at.path).join("gemini"), "").unwrap();
        assert_eq!(missing(&at), [Cli::Codex, Cli::Kimi]);
        // a directory of that name is not the command
        std::fs::create_dir(Path::new(&at.path).join("codex")).unwrap();
        assert_eq!(missing(&at), [Cli::Codex, Cli::Kimi]);
        cleanup(&at);
    }

    /// kimi's installer keeps the command in kimi's own directory,
    /// which the PATH of an app need not include.
    #[test]
    fn agents_count_kimi_where_its_installer_puts_it() {
        let at = places("kimi");
        std::fs::create_dir_all(at.home.join(".kimi-code/bin")).unwrap();
        assert!(missing(&at).contains(&Cli::Kimi), "a home with no command in it");
        std::fs::write(kimi::bin(&at.home), "").unwrap();
        assert_eq!(missing(&at), [Cli::Claude, Cli::Codex, Cli::Gemini]);
        cleanup(&at);
    }

    #[test]
    fn agents_count_the_packaged_codex_as_present() {
        let at = places("packaged");
        let root = at.codex_home.join("packages/standalone");
        let release = root.join("releases/0.164.0-aarch64-apple-darwin");
        std::fs::create_dir_all(&release).unwrap();
        std::os::unix::fs::symlink(&release, root.join("current")).unwrap();
        assert_eq!(missing(&at), [Cli::Claude, Cli::Gemini, Cli::Kimi]);
        cleanup(&at);
    }

    #[test]
    fn agents_retry_a_failed_install_a_day_later_and_not_before() {
        let mut failed = Failures::default();
        let t0 = Instant::now();
        assert!(failed.due(Cli::Gemini, t0));
        failed.record(Cli::Gemini, t0, false);
        assert!(!failed.due(Cli::Gemini, t0 + RETRY_AFTER / 2));
        assert!(!failed.due(Cli::Gemini, t0 + RETRY_AFTER - Duration::from_secs(1)));
        assert!(failed.due(Cli::Gemini, t0 + RETRY_AFTER));
        // one CLI's failure holds back no other
        assert!(failed.due(Cli::Claude, t0 + EVERY));
        // a success forgets the failure
        failed.record(Cli::Gemini, t0 + RETRY_AFTER, true);
        assert!(failed.due(Cli::Gemini, t0 + RETRY_AFTER));
    }

    /// The configuration that follows an install is written by workers
    /// the supervisor's tick asks, and the supervisor may be at rest.
    #[test]
    fn an_install_that_finished_is_noted_as_a_change_and_a_failed_one_is_not() {
        let before = changes::count();
        finished(Cli::Claude, &Err("curl: (6) could not resolve host".into()));
        assert_eq!(changes::count(), before);
        finished(Cli::Claude, &Ok("2.1.0 (Claude Code)".into()));
        assert_eq!(changes::count(), before + 1);
    }

    /// A shell profile prints what it likes around the answer.
    #[test]
    fn the_path_is_read_from_between_the_marks() {
        let out = "Welcome!\n\u{1b}[0m__m__/opt/homebrew/bin:/usr/bin__m__\nbye";
        assert_eq!(path_between_marks(out, "__m__").as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(path_between_marks("no marks at all", "__m__"), None);
        assert_eq!(path_between_marks("__m____m__", "__m__"), None);
    }
}
