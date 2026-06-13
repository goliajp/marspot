//! Single source of truth for marspot's on-disk state locations.
//!
//! `MARSPOT_STATE_DIR` overrides the root.  That one knob is what
//! separates the *installed* app (default root, the terminal the
//! user lives in) from every dev / test world: `bin/test-*.sh` and
//! `bin/run.sh` export their own state dir, run their own shelld on
//! their own socket, and can kill processes / wipe state freely
//! without ever touching the production instance.
//!
//! Everything that names a path under the state root MUST come
//! through here — a hardcoded `~/Library/Caches/marspot` anywhere
//! else is a sandbox escape.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// State root: `$MARSPOT_STATE_DIR`, else
/// `~/Library/Caches/marspot` (the installed app's world).
pub fn state_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("MARSPOT_STATE_DIR") {
        return PathBuf::from(dir);
    }
    home().join("Library/Caches/marspot")
}

/// Log directory.  The installed app logs to the conventional
/// `~/Library/Logs/Marspot`; sandboxed runs keep logs inside their
/// state root so one `rm -rf` cleans everything.
pub fn log_dir() -> PathBuf {
    if std::env::var_os("MARSPOT_STATE_DIR").is_some() {
        return state_root().join("logs");
    }
    home().join("Library/Logs/Marspot")
}

pub fn supervisor_log() -> PathBuf {
    log_dir().join("supervisor.log")
}

/// shelld control socket.  One socket per state root = one daemon
/// per world; the sandbox shelld never sees production sessions.
/// `MARSPOT_SHELLD_SOCKET` forces an explicit path (highest
/// priority) for the rare case a caller wants the socket somewhere
/// other than under the state root.
pub fn shelld_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("MARSPOT_SHELLD_SOCKET") {
        return PathBuf::from(p);
    }
    state_root().join("shelld.sock")
}

/// shelld per-session storage (bytelogs).
pub fn sessions_dir() -> PathBuf {
    state_root().join("sessions")
}

/// Supervisor binary slots (current/prev/pending/quarantine).
pub fn binaries_root() -> PathBuf {
    state_root().join("binaries")
}

/// Shell crash-loop launch journal.
pub fn shell_launch_journal() -> PathBuf {
    state_root().join("shell_launches.tsv")
}

/// Running GUI shell's pid file — lets `--trigger` / `--status` /
/// scripts target the shell of *this* state root instead of
/// whichever marspot-shell process `ps` happens to list first.
pub fn shell_pid_file() -> PathBuf {
    state_root().join("shell.pid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_is_under_home_caches() {
        if std::env::var_os("MARSPOT_STATE_DIR").is_none()
            && std::env::var_os("MARSPOT_SHELLD_SOCKET").is_none()
        {
            assert!(state_root().ends_with("Library/Caches/marspot"));
            assert!(log_dir().ends_with("Library/Logs/Marspot"));
            assert!(shelld_socket().starts_with(state_root()));
        }
    }

    #[test]
    fn children_live_under_root() {
        // With no socket override, every child stays under the root —
        // the property that makes a sandbox state dir leak-proof.
        if std::env::var_os("MARSPOT_SHELLD_SOCKET").is_some() {
            return;
        }
        let root = state_root();
        for p in [
            shelld_socket(),
            sessions_dir(),
            binaries_root(),
            shell_launch_journal(),
            shell_pid_file(),
        ] {
            assert!(p.starts_with(&root), "{p:?} escapes {root:?}");
        }
    }
}
