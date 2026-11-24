//! The part of `share` after the busy check: the home's lock directory
//! is replaced by the link, then its lock files go into the shared
//! directory.
//!
//! `busy` only looked; a codex of the home can start between that look
//! and this.  Codex takes the directory's `.coordination.lock` before it
//! makes or takes a thread's lock file (a `thread/start` waits while
//! another process holds it), so that one and every lock file are taken
//! first; either not to be had is a busy home, and nothing has been
//! changed yet.  A lock follows the file, not its name, so the link
//! takes the directory's place in one swap before any entry moves: a
//! codex that starts after it finds the shared directory through the
//! link, and one already waiting on the old coordination lock opens its
//! thread's file by name, through the link too.  Each lock is let go
//! once its file has its place, the coordination lock last.

use super::{COORD, DIR, LEFT, Outcome};
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// What `move_into_seen` reports as it goes, for the tests.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step<'a> {
    /// Every lock is taken; nothing has moved yet.
    Locked,
    /// The link is where the directory was.
    Linked,
    /// The entry has its place (moved, dropped or left) and its lock
    /// is let go.
    Placed(&'a OsStr),
}

fn try_lock(f: &File) -> bool {
    // SAFETY: a valid fd
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn renamex(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let (a, b) = (CString::new(from.as_os_str().as_bytes())?, CString::new(to.as_os_str().as_bytes())?);
    // SAFETY: two nul-terminated paths
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Move `home`'s lock directory (when `exists`) into `target` and link
/// it there.
pub(super) fn move_into(home: &Path, target: &Path, exists: bool) -> Outcome {
    move_into_seen(home, target, exists, &mut |_| {})
}

pub(super) fn move_into_seen(home: &Path, target: &Path, exists: bool, seen: &mut dyn FnMut(Step<'_>)) -> Outcome {
    let dir = home.join(DIR);
    if !exists {
        return match symlink(target, &dir) {
            Ok(()) => Outcome::Shared { moved: 0, duplicates: 0, left: 0 },
            Err(e) => Outcome::Failed(format!("{}: {e}", dir.display())),
        };
    }
    let (coord, entries) = match lock_all(&dir) {
        Ok(v) => v,
        Err(o) => return o,
    };
    seen(Step::Locked);
    // the link is made under a name of its own and swapped in, so the
    // directory's name is never missing; the name then is the old one's
    let old = home.join(format!(".{DIR}.moving-{}", std::process::id()));
    if let Err(e) = symlink(target, &old) {
        return Outcome::Failed(format!("{}: {e}", old.display()));
    }
    if let Err(e) = renamex(&old, &dir, libc::RENAME_SWAP) {
        let _ = std::fs::remove_file(&old);
        return Outcome::Failed(format!("{}: {e}", dir.display()));
    }
    seen(Step::Linked);
    let mut counts = (0, 0, 0);
    for (name, lock) in entries.into_iter().chain([(OsString::from(COORD), Some(coord))]) {
        if let Err(e) = place(&old.join(&name), &target.join(&name), &mut counts) {
            return Outcome::Failed(format!("{}: {e}; the rest is in {}", name.display(), old.display()));
        }
        drop(lock);
        seen(Step::Placed(&name));
    }
    let (moved, duplicates, left) = counts;
    let gone = if left == 0 { std::fs::remove_dir(&old) } else { std::fs::rename(&old, leftover_dir(home)) };
    match gone {
        Ok(()) => Outcome::Shared { moved, duplicates, left },
        Err(e) => Outcome::Failed(format!("{}: {e}", old.display())),
    }
}

/// The coordination lock and every other entry of `dir`, each regular
/// file locked.
fn lock_all(dir: &Path) -> Result<(File, Vec<(OsString, Option<File>)>), Outcome> {
    let coord = dir.join(COORD);
    let c = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&coord);
    let c = c.map_err(|e| Outcome::Failed(format!("{}: {e}", coord.display())))?;
    if !try_lock(&c) {
        return Err(Outcome::Busy(format!("{} is held", coord.display())));
    }
    let rd = std::fs::read_dir(dir).map_err(|e| Outcome::Failed(format!("{}: {e}", dir.display())))?;
    let mut out = Vec::new();
    for e in rd {
        let e = e.map_err(|e| Outcome::Failed(e.to_string()))?;
        let (p, name) = (e.path(), e.file_name());
        if name == COORD {
            continue;
        }
        let mut lock = None;
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            let f = File::open(&p).map_err(|e| Outcome::Failed(format!("{}: {e}", p.display())))?;
            if !try_lock(&f) {
                return Err(Outcome::Busy(format!("{} is held", p.display())));
            }
            lock = Some(f);
        }
        out.push((name, lock));
    }
    Ok((c, out))
}

/// Move `src` to `dst` unless `dst` exists; it then stays: it may be
/// the very file another home's codex holds, and a lock file carries
/// nothing but its lock.  `src` is then dropped when it is an empty
/// file, left where it is otherwise.  The rename keeps the inode, and
/// any lock on it with it, and never replaces a `dst` made meanwhile.
fn place(src: &Path, dst: &Path, (moved, duplicates, left): &mut (usize, usize, usize)) -> std::io::Result<()> {
    match renamex(src, dst, libc::RENAME_EXCL) {
        Ok(()) => *moved += 1,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(src).is_ok_and(|m| m.is_file() && m.len() == 0) {
                std::fs::remove_file(src)?;
                *duplicates += 1;
            } else {
                *left += 1;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// The number of a `LEFT` directory name: 1 for `LEFT`, n for `LEFT.n`.
fn left_number(name: &OsStr) -> Option<usize> {
    let name = name.to_str()?;
    if name == LEFT {
        return Some(1);
    }
    name.strip_prefix(LEFT)?.strip_prefix('.')?.parse().ok().filter(|n| *n > 1)
}

/// `LEFT`, or `LEFT.n` one above every number already there.
fn leftover_dir(home: &Path) -> PathBuf {
    match leftovers(home).iter().filter_map(|p| p.file_name().and_then(left_number)).max() {
        None => home.join(LEFT),
        Some(n) => home.join(format!("{LEFT}.{}", n + 1)),
    }
}

/// Every `LEFT` directory in `home`, by number.
pub(super) fn leftovers(home: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(home) else { return Vec::new() };
    let mut v: Vec<(usize, PathBuf)> =
        rd.flatten().filter_map(|e| left_number(&e.file_name()).map(|n| (n, e.path()))).collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}
