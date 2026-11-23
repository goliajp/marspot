//! The mark of a session the app closed: an empty file in its directory.
//!
//! Only a close writes it, before the session is signalled, and only
//! with it does a hangup end a session's history: a hangup can come
//! from anyone.  A core that starts and finds the mark finishes the
//! close; it neither attaches to the session nor resurrects it.

use std::io;
use std::path::PathBuf;

use super::{list_session_dir_ids, session_dir};

fn mark(id: u64) -> PathBuf {
    session_dir(id).join("closed")
}

/// Mark the session as closed.  Never makes its directory.
pub fn mark_closed(id: u64) -> io::Result<()> {
    std::fs::File::create(mark(id)).map(|_| ())
}

/// The directory has a session of its own again: the mark was of an
/// earlier one, whose close was cut short.
pub fn clear_closed(id: u64) -> io::Result<()> {
    match std::fs::remove_file(mark(id)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        done => done,
    }
}

pub fn is_closed(id: u64) -> bool {
    mark(id).exists()
}

/// Every session that was closed and whose directory is still there.
pub fn closed_session_ids() -> Vec<u64> {
    let mut ids = list_session_dir_ids();
    ids.retain(|id| is_closed(*id));
    ids
}

#[cfg(test)]
mod tests {
    use super::super::test_support::sample_entry;
    use super::super::{list_session_entries, write_session_entry};
    use super::*;
    use crate::test_env::sandbox_state_dir;

    #[test]
    fn a_closed_session_is_listed_as_closed_and_as_nothing_else() {
        let _sandbox = sandbox_state_dir();
        assert!(mark_closed(5).is_err(), "there is no such session");
        assert!(!session_dir(5).exists(), "and none is made");
        for id in [5, 6] {
            write_session_entry(&sample_entry(id)).unwrap();
        }
        mark_closed(5).unwrap();
        assert_eq!(closed_session_ids(), [5]);
        assert!(is_closed(5) && !is_closed(6));
        let listed: Vec<u64> = list_session_entries().iter().map(|e| e.id).collect();
        assert_eq!(listed, [6]);
        clear_closed(5).unwrap();
        clear_closed(6).unwrap();
        assert!(closed_session_ids().is_empty(), "a session that starts there is not closed");
    }
}
