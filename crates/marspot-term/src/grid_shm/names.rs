//! What a session's shared regions are called.
//!
//! Session ids are counted per state directory and every directory
//! starts at 1, while shared-memory names are one namespace for the
//! whole login.  So the name carries a digest of the state directory:
//! two apps on one machine, each with its own directory, never ask for
//! the same name.
//!
//! The cluster and title regions are named after the grid region, by
//! swapping the letter in its prefix.  That holds for a name from
//! before the digest too (`/msp-s-7` gives `/msp-c-7`), so a session
//! started by an older build is found where that build put it.

use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::Mutex;

const GRID_PREFIX: &str = "/msp-s-";

/// Which of a session's side regions.
#[derive(Clone, Copy)]
pub(super) enum Side {
    Clusters,
    Title,
}

/// A digest of the state directory this process runs under.
///
/// Of the resolved path, so `/tmp/x` and `/private/tmp/x` are one
/// directory.  Remembered per spelling: a reader that asks on every
/// tick must not resolve a path each time.
pub fn state_hash() -> u32 {
    static LAST: Mutex<Option<(PathBuf, u32)>> = Mutex::new(None);
    let root = crate::paths::state_root();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((spelling, hash)) = last.as_ref()
        && *spelling == root
    {
        return *hash;
    }
    match std::fs::canonicalize(&root) {
        Ok(resolved) => {
            let hash = fnv1a(resolved.as_os_str().as_encoded_bytes());
            *last = Some((root, hash));
            hash
        }
        // not there yet: nothing to remember, the next caller resolves
        Err(_) => fnv1a(root.as_os_str().as_encoded_bytes()),
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |h, b| (h ^ *b as u32).wrapping_mul(0x0100_0193))
}

/// The grid region's name for session `id` of this state directory:
/// `/msp-s-<digest, 8 hex>.<id, base 36>`, at most 29 bytes where
/// macOS allows 31.
pub fn session_shm_name(id: u64) -> CString {
    CString::new(format!("{GRID_PREFIX}{:08x}.{}", state_hash(), base36(id))).expect("no NUL")
}

fn base36(mut n: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("ascii")
}

/// The name of a side region of the session whose grid region is
/// `grid`.  `None` for a grid region not named as a session's: nobody
/// could find a side region of it.
pub(super) fn side_name(grid: &CStr, side: Side) -> Option<CString> {
    let rest = grid.to_str().ok()?.strip_prefix(GRID_PREFIX)?;
    let letter = match side {
        Side::Clusters => 'c',
        Side::Title => 't',
    };
    CString::new(format!("/msp-{letter}-{rest}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_name_fits_the_system_limit() {
        let name = session_shm_name(u64::MAX);
        assert!(name.as_bytes().len() <= 31, "{name:?}");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    fn side_regions_are_named_after_the_grid_region() {
        let grid = CString::new("/msp-s-0a1b2c3d.1z").unwrap();
        assert_eq!(side_name(&grid, Side::Clusters).unwrap().to_str(), Ok("/msp-c-0a1b2c3d.1z"));
        assert_eq!(side_name(&grid, Side::Title).unwrap().to_str(), Ok("/msp-t-0a1b2c3d.1z"));
        // where a build from before the digest put them
        let old = CString::new("/msp-s-7").unwrap();
        assert_eq!(side_name(&old, Side::Clusters).unwrap().to_str(), Ok("/msp-c-7"));
        assert!(side_name(&CString::new("/msp-g-12-0").unwrap(), Side::Title).is_none());
    }
}
