//! Taking a session's directory away: for good, or into the recycle dir.
//!
//! Either way the directory leaves `sessions/` first, by a rename.  A
//! thread of the session that still creates files there by path finds
//! no directory, so nothing can appear in it while it is emptied; a
//! removal in place failed on such a file and left the rest behind.  A
//! removal cut short leaves it in the recycle dir, which is purged.

use std::io;
use std::path::PathBuf;

use super::{session_dir, try_lock_session_dir};

/// Out of `sessions/` into the recycle dir, as `<name>-<unix-secs>`.
fn move_away(id: u64, name: &str) -> io::Result<PathBuf> {
    let root = crate::paths::state_root().join("retired");
    std::fs::create_dir_all(&root)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = root.join(format!("{name}-{ts}"));
    std::fs::rename(session_dir(id), &dst)?;
    Ok(dst)
}

/// Take the session's directory away to be deleted by the caller;
/// `None` when there is none.
pub fn take_session_dir(id: u64) -> io::Result<Option<PathBuf>> {
    match move_away(id, &format!("{id}.{}", std::process::id())) {
        Ok(dst) => Ok(Some(dst)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove this session's whole directory tree (entry.toml + sock +
/// bytelog).
pub fn delete_session(id: u64) -> io::Result<()> {
    match take_session_dir(id)? {
        Some(dir) => std::fs::remove_dir_all(dir),
        None => Ok(()),
    }
}

/// `f` on the session's directory, unless there is none or a live L3
/// holds it.  True when `f` ran.
fn unless_held<T>(id: u64, f: impl FnOnce(u64) -> io::Result<T>) -> io::Result<bool> {
    if !session_dir(id).exists() {
        return Ok(false);
    }
    match try_lock_session_dir(id) {
        Ok(_held) => f(id).map(|_| true),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

/// [`delete_session`], unless a live L3 holds the directory.  True when
/// there was one and it is gone.
pub fn delete_dead_session(id: u64) -> io::Result<bool> {
    unless_held(id, delete_session)
}

/// [`retire_session_dir`], unless a live L3 holds the directory.  True
/// when there was one and it went.
pub fn retire_dead_session(id: u64) -> io::Result<bool> {
    unless_held(id, retire_session_dir)
}

/// Move a session's directory to `state_root()/retired/<id>-<unix-secs>/`
/// instead of deleting it.  History stays recoverable for
/// `RETIRED_TTL_SECS`; the purge pass below removes expired entries.
/// Only a session the user closed, or whose shell ended, is deleted.
pub fn retire_session_dir(id: u64) -> io::Result<PathBuf> {
    move_away(id, &id.to_string())
}

/// How long a retired session dir stays on disk before the purge
/// pass removes it.  14 days — long enough to notice "my pane's
/// history vanished" and recover, short enough to bound disk use.
pub const RETIRED_TTL_SECS: u64 = 14 * 24 * 3600;

/// Remove retired dirs older than `RETIRED_TTL_SECS` (age read from
/// the `-<unix-secs>` name suffix — immune to mtime updates from
/// forensic reads).  Returns how many were purged.  Best-effort:
/// unparseable names are left alone.
pub fn purge_expired_retired() -> usize {
    let root = crate::paths::state_root().join("retired");
    let Ok(read_dir) = std::fs::read_dir(&root) else {
        return 0;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut purged = 0usize;
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((_, ts)) = name.rsplit_once('-') else {
            continue;
        };
        let Ok(ts) = ts.parse::<u64>() else { continue };
        if now.saturating_sub(ts) > RETIRED_TTL_SECS
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            purged += 1;
        }
    }
    purged
}
