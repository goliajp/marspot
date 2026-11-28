//! Where the codex installer comes from: codex's own, fetched.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::update::tail;

/// The command codex itself prints when `codex update` cannot tell how
/// it was installed, which is the case on a machine whose packages
/// directory is shared between profiles.
const INSTALLER: &str = "https://chatgpt.com/codex/install.sh";

/// Dev seam: an installer script run in place of codex's, for a test on
/// a machine that must not download a real CLI.  Honoured only by a dev
/// build or under a sandbox state dir, like `MARSPOT_DEV_AGENTS_FROM`.
const DEV_INSTALLER: &str = "MARSPOT_DEV_CODEX_INSTALLER";

/// The installer to run, fetched into `dir` unless the dev seam names one.
pub(super) fn script(dir: &Path) -> Result<PathBuf, String> {
    let sandbox = std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
    if (cfg!(debug_assertions) || sandbox)
        && let Some(p) = std::env::var_os(DEV_INSTALLER).filter(|p| !p.is_empty())
    {
        return Ok(PathBuf::from(p));
    }
    let script = dir.join("install.sh");
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
    Ok(script)
}
