//! The control socket this process bound, told apart from one bound at
//! the same path by whoever owns the session's directory now: the
//! directory can be taken away and made again while this process runs.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SocketId {
    dev: u64,
    ino: u64,
}

impl SocketId {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let m = std::fs::symlink_metadata(path).ok()?;
        Some(Self { dev: m.dev(), ino: m.ino() })
    }
}

/// Unlink the socket at `path` when it is still the one `ours` names.
pub(crate) fn remove_if_ours(path: &Path, ours: Option<SocketId>) -> bool {
    ours.is_some() && SocketId::of(path) == ours && std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn close_removes_only_the_socket_it_bound() {
        // not in a sandbox, whose path can be longer than a socket's may be
        let path = std::path::PathBuf::from(format!("/tmp/mso-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _first = UnixListener::bind(&path).unwrap();
        let first = SocketId::of(&path);
        std::fs::remove_file(&path).unwrap();
        let _second = UnixListener::bind(&path).unwrap();
        assert!(!remove_if_ours(&path, first), "another's socket at the same path");
        assert!(!remove_if_ours(&path, None));
        assert!(path.exists());
        assert!(remove_if_ours(&path, SocketId::of(&path)));
        assert!(!path.exists());
    }
}
