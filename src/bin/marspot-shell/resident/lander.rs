//! The bundle lander: what lands a staged app bundle once this pid is gone.

use std::os::unix::process::CommandExt;

use marspot::{lx_event, lx_warn};

/// Start the armed bundle lander, if there is one, and hand it the one
/// moment it can work in.
///
/// `install-local.sh` cannot write `Marspot.app/Contents/MacOS/` while
/// a process is running that binary: the system kills a process whose
/// file on disk stops matching what it was started from.  So a changed
/// shell is staged behind a one-shot `land.sh`, which waits for this
/// pid to be gone.  Its LaunchAgent at login is only a backstop: the
/// app is what the user opens first, so that copy finds it running and
/// gives up.
///
/// Detached into its own process group so the exit right after this
/// does not take it along.  Every failure is silent on purpose: nothing
/// about quitting may depend on an update landing.
pub(crate) fn land_staged_bundle() {
    let script = marspot_term::paths::state_root().join("pending-bundle").join("land.sh");
    if !script.exists() {
        return;
    }
    let pid = std::process::id();
    let spawned = std::process::Command::new(&script)
        .arg(pid.to_string())
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => lx_event!(
            "BUNDLE_LANDER_START",
            "handed the armed bundle lander this exit to land in",
            lander_pid = child.id(),
            waits_for = pid
        ),
        Err(e) => lx_warn!(
            "shell.quit.lander_spawn_failed",
            &format!("{e} — the stage stays armed for the login backstop")
        ),
    }
}
