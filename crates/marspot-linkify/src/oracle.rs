//! The path oracle: how a scan asks whether a candidate path exists.

use std::path::PathBuf;

/// Turn a link's text into a path the OS will accept.
///
/// `~` is shell syntax, not filesystem syntax: nothing below the shell
/// expands it.  This matters beyond the existence check — whoever ACTS
/// on a file link (opening it, revealing it) has to hand the same
/// expanded path to the OS, or the link resolves here and fails there.
/// That was live until 2026-08-21: every `~/…` file link stat'ed fine,
/// underlined, and then did nothing when opened, because
/// `/usr/bin/open` took `~` for a directory name and looked for it
/// under the process's cwd.
///
/// Returns `None` only when `~/` is present and `HOME` is not — there
/// is no sensible path to hand back in that case.
pub fn expand_user_path(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|home| {
            let mut p = PathBuf::from(home);
            p.push(rest);
            p
        }),
        None => Some(PathBuf::from(path)),
    }
}

/// What the oracle knows about one candidate path.
///
/// `Unknown` exists so an oracle can answer without touching the
/// filesystem.  `scan_visible_links` runs inside `build_instances`,
/// once per pane per frame; an oracle that blocks on `lstat` there
/// puts a syscall — and a page-cache miss, and whatever the disk is
/// doing — on the render thread.  On 2026-08-22 that cost a live
/// 14-pane window frames of 2.1 s to 13.6 s (`l2.loop.stall`,
/// build-bound), with 79.5 % of render self-time in `lstat` under
/// this call.  An async oracle answers `Unknown` on a miss, queues
/// the probe, and the link appears a frame or two later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathVerdict {
    /// The path resolves to a filesystem entry.
    Exists,
    /// The path was checked and does not resolve.
    Missing,
    /// Not checked yet.  Never blocks; treated as "no link, for now".
    Unknown,
}

/// Decides whether a candidate path is real.  Injected so the stone
/// crate itself performs no I/O: the scanner is pure over
/// (cells, opts, oracle), which is also what makes it testable
/// without a filesystem.
pub trait PathOracle {
    fn probe(&self, path: &str) -> PathVerdict;
}

/// The blocking oracle: one `symlink_metadata` per call, no cache.
///
/// Correct for one-shot callers (tests, `scan_text_line`, snapshot
/// rendering) where a handful of stats is cheaper than any cache.
/// NOT for the render loop — see [`PathVerdict::Unknown`].
pub struct FsOracle;

impl PathOracle for FsOracle {
    fn probe(&self, path: &str) -> PathVerdict {
        match expand_user_path(path) {
            Some(expanded) if std::fs::symlink_metadata(&expanded).is_ok() => PathVerdict::Exists,
            _ => PathVerdict::Missing,
        }
    }
}

/// An oracle that answers `Unknown` for everything — no link is ever
/// a file link.  For callers that want URL/email/IP detection with
/// zero filesystem access.
pub struct NoFsOracle;

impl PathOracle for NoFsOracle {
    fn probe(&self, _path: &str) -> PathVerdict {
        PathVerdict::Unknown
    }
}
