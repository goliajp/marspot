//! What the question before a CLI's first install says about it, the
//! wake an answer of yes sends the installer, and the dev seam that
//! installs from local scripts instead of the vendors'.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;

use marspot::settings::{InstallConsent, InstallConsents};
use marspot::shell_proto::InstallAsk;
use marspot_term::lx_info;

use super::{present, run, Cli, Places};
use crate::agentsvc::install::{self, Gate};

/// The question for `cli`: where its installer puts it and about how
/// much room it takes.  The sizes are what each took on a Mac that had
/// all four in November 2026 (claude 384 MB, codex 736 MB, gemini
/// 122 MB, kimi 371 MB), rounded, and the question says "about": a
/// vendor's next build may differ.
pub(super) fn ask_for(cli: Cli, at: &Places) -> InstallAsk {
    let (place, megabytes) = match cli {
        // the native build's versions; the command is a link in ~/.local/bin
        Cli::Claude => ("~/.local/share/claude + ~/.local/bin".to_string(), 380),
        Cli::Codex => (format!("{}/packages + ~/.local/bin", tilde(&at.codex_home, &at.home)), 740),
        Cli::Gemini => ("~/.local/bin".to_string(), 120),
        Cli::Kimi => ("~/.kimi-code".to_string(), 370),
    };
    InstallAsk { cli: cli.command().to_string(), place, megabytes }
}

/// `path` with `home` written as `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Whether the missing `cli` may be installed: the person said yes for
/// it.  Without an answer the question is asked and nothing is fetched.
pub(super) fn may_install(cli: Cli, at: &Places, answers: &InstallConsents) -> bool {
    match install::gate(ask_for(cli, at), answers) {
        Gate::Install => true,
        Gate::Leave => false,
        Gate::Asked => {
            lx_info!("agents.asked", "waiting for a yes before the first install", cli = cli.command());
            false
        }
    }
}

/// Whether marspot may update `cli` where it is: not after a no.
pub(crate) fn may_update(cli: &str, answers: &InstallConsents) -> bool {
    answers.of(cli) != Some(InstallConsent::No)
}

/// Dev seam: a directory of `install-<cli>.sh` scripts run in place of
/// every vendor's installer, and no update is looked for.  For a test
/// on a machine that must not download a real CLI.  Honoured only by a
/// dev build or under a sandbox state dir (`MARSPOT_STATE_DIR`), so an
/// installed app is never pointed at someone's scripts.
const FIXTURES_VAR: &str = "MARSPOT_DEV_AGENTS_FROM";

pub(super) fn fixtures() -> Option<PathBuf> {
    let sandbox = std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
    if !cfg!(debug_assertions) && !sandbox {
        return None;
    }
    std::env::var_os(FIXTURES_VAR).map(PathBuf::from)
}

pub(super) fn install_fixture(dir: &std::path::Path, cli: Cli, at: &Places) -> Result<String, String> {
    let script = dir.join(format!("install-{}.sh", cli.command()));
    run(Command::new("/bin/bash").arg(&script).env("HOME", &at.home), "fixture installer")?;
    if present(cli, at) { Ok("fixture".to_string()) } else { Err(format!("{} installed no {}", script.display(), cli.command())) }
}

/// The installer thread's wake; its only sender, so taking it away ends
/// the thread.
static WAKE: Mutex<Option<SyncSender<()>>> = Mutex::new(None);
/// The answers the last look saw.
static SEEN: Mutex<Option<InstallConsents>> = Mutex::new(None);

/// Keeps the installer thread running; dropped when the app is put away.
pub(crate) struct Running(());

impl Drop for Running {
    fn drop(&mut self) {
        WAKE.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

pub(super) fn wake_channel() -> (Running, Receiver<()>) {
    let (tx, rx) = sync_channel(1);
    *WAKE.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
    (Running(()), rx)
}

/// On the supervisor's tick: a yes that was not there before starts
/// the install now, not at the installer's next pass an hour away.
pub(crate) fn follow_answers() {
    let now = marspot::settings::get().install_consent;
    let before = SEEN.lock().unwrap_or_else(|p| p.into_inner()).replace(now);
    if before.is_some_and(|b| install::newly_yes(&b, &now))
        && let Some(tx) = WAKE.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
    {
        // full means a pass is already coming
        let _ = tx.try_send(());
    }
}

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;
