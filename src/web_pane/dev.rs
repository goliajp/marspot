//! The web pane's dev seams, honoured only by a dev build or under a
//! sandbox state dir, as the other `MARSPOT_DEV_*` seams are.

use std::path::PathBuf;

/// Dev seam: open one web pane on this file URL after start.  Honoured
/// only by a dev build or under a sandbox state dir, as the other
/// `MARSPOT_DEV_*` seams that point the app at a developer's files.
pub fn dev_url() -> Option<&'static str> {
    // read once: the core asks every main-loop turn
    static URL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    URL.get_or_init(|| dev_var("MARSPOT_DEV_WEB_PANE")).as_deref()
}

/// Whether the ways a person opens a web pane (the menus, Command and a
/// click on a link) are offered: not until the page's content process
/// runs in a sandbox, so only where the dev seams are honoured and
/// `MARSPOT_DEV_WEB_ENTRY` asks for them.
pub fn entry_points() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| dev_var("MARSPOT_DEV_WEB_ENTRY").is_some())
}

/// Dev seam: the host program a sandbox starts, in place of the one
/// installed at the fixed path.
pub fn dev_host_program() -> Option<PathBuf> {
    dev_var("MARSPOT_DEV_MADCAWL_SHELL").map(PathBuf::from)
}

pub(super) fn dev_var(name: &str) -> Option<String> {
    let sandbox = std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
    if !cfg!(debug_assertions) && !sandbox {
        return None;
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
