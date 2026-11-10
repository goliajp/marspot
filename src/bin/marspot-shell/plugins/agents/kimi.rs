//! kimi, from its vendor's install script.
//!
//! The script needs only curl, tar and shasum, checks what it
//! downloads against the release's manifest, and puts one signed
//! executable in `~/.kimi-code/bin`.  kimi updates itself from there,
//! so the location is the vendor's and not `~/.local/bin`; the pane's
//! PATH has both.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{fetch, run, scratch, version};

const INSTALLER: &str = "https://code.kimi.com/kimi-code/install.sh";

/// Where the installer puts the command.
pub(super) fn bin(home: &Path) -> PathBuf {
    home.join(".kimi-code/bin/kimi")
}

pub(super) fn install(home: &Path) -> Result<String, String> {
    let dir = scratch("kimi")?;
    let script = dir.join("install.sh");
    let r = fetch(INSTALLER, &script).and_then(|()| {
        // a shell profile is the user's file; the pane's PATH is set
        // without one
        run(Command::new("/bin/bash").arg(&script).env("HOME", home).env("KIMI_NO_MODIFY_PATH", "1"), "installer")?;
        version(&bin(home))
    });
    let _ = std::fs::remove_dir_all(&dir);
    r
}
