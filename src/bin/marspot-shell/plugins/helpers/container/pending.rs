//! The containers that may still be there: a file for each helper in
//! `containers/` beside the helpers' directories, made before its
//! container can be and removed once docker said the container is gone.
//! Until then the helper's directory stays, as the container may write
//! into it.  `containers/` itself stays: it says there ever was one.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::super::dirs;

pub(super) fn place(root: &Path) -> PathBuf {
    root.join("containers")
}

fn file_of(root: &Path, sid: u64) -> PathBuf {
    place(root).join(sid.to_string())
}

/// Say that helper `sid` may have a container, which may write into the
/// directories `given`.
pub(super) fn add(root: &Path, sid: u64, given: &[&Path]) -> std::io::Result<()> {
    std::fs::create_dir_all(place(root))?;
    // no path has a zero byte in it
    let body = given.iter().map(|p| p.as_os_str().as_bytes()).collect::<Vec<_>>().join(&0);
    std::fs::write(file_of(root, sid), body)
}

pub(in super::super) fn pending(root: &Path, sid: u64) -> bool {
    file_of(root, sid).symlink_metadata().is_ok()
}

pub(super) fn all(root: &Path) -> HashSet<u64> {
    let rd = std::fs::read_dir(place(root)).into_iter().flatten().flatten();
    rd.filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// Helper `sid`'s container is gone: what it left in the directories it
/// was given is seen to, and its own goes.
pub(super) fn gone(root: &Path, sid: u64) {
    let file = file_of(root, sid);
    let body = std::fs::read(&file).unwrap_or_default();
    for dir in body.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        set_aside(Path::new(OsStr::from_bytes(dir)), sid);
    }
    if let Err(e) = std::fs::remove_file(&file)
        && e.kind() != ErrorKind::NotFound
    {
        marspot::lx_warn!("helpers.container_pending", &format!("{}: {e}", file.display()));
    }
    dirs::remove(root, sid);
}

/// Move what is no regular file in `dir`, a directory of a login, beside
/// it: the agent on the host would follow it, or wait for it.
pub(super) fn set_aside(dir: &Path, sid: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(format!("-set-aside-{sid}"));
    let aside = dir.with_file_name(name);
    for e in rd.flatten() {
        // the directories are the agent's locks
        let fits = e.file_type().is_ok_and(|t| t.is_file() || t.is_dir() && e.file_name().to_string_lossy().ends_with(".lock"));
        if fits {
            continue;
        }
        let moved = std::fs::create_dir_all(&aside).and_then(|()| std::fs::rename(e.path(), aside.join(e.file_name())));
        marspot::lx_warn!("helpers.container_set_aside", &format!("{} to {}: {moved:?}", e.path().display(), aside.display()));
    }
}
