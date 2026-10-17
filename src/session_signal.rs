//! Signalling an L3 through its registry entry.
//!
//! An entry's pid is the L3 only while `entry_is_live` says so: after
//! the L3 exits, the number can name any process on the machine.

use marspot_term::session_registry::{SessionEntry, entry_is_live, read_session_entry};

use crate::pidtree::{self, ProcId, SignalError};

/// Send `sig` to the L3 that wrote `entry`, and to nothing else.
/// `Err(Gone)` when the entry's pid is no longer that L3.
pub fn signal_entry(entry: &SessionEntry, sig: i32) -> Result<ProcId, SignalError> {
    // identity first: a pid reused before the liveness check fails it,
    // and one reused after it fails `signal`
    let id = pidtree::proc_id(entry.pid).ok_or(SignalError::Gone)?;
    if !entry_is_live(entry) {
        return Err(SignalError::Gone);
    }
    pidtree::signal(id, sig).map(|()| id)
}

/// [`signal_entry`] for the entry of session `sid`; `Err(Gone)` when
/// it has none that reads.
pub fn signal_session(sid: u64, sig: i32) -> Result<ProcId, SignalError> {
    let entry = read_session_entry(sid).map_err(|_| SignalError::Gone)?;
    signal_entry(&entry, sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::session_registry::write_session_entry;

    /// An entry whose pid is a live process other than an L3 (a
    /// recycled pid) is refused, and that process is left alone.
    #[test]
    fn an_entry_whose_pid_is_not_an_l3_is_not_signalled() {
        let sandbox = marspot_term::test_env::sandbox_state_dir();
        let dir = sandbox.path();
        let mut child = marspot_term::test_env::ChildGuard::new(
            std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap(),
        );
        let entry = SessionEntry {
            id: 7,
            pid: child.id() as i32,
            socket: dir.join("sock"),
            cols: 80,
            rows: 24,
            title: String::new(),
            cwd: "/tmp".into(),
            proto_version: marspot_term::shell_proto::PROTO_VERSION,
            created_at_unix: 0,
            shm_name: String::new(),
            shell_child_pid: 0,
        };
        write_session_entry(&entry).unwrap();
        let by_entry = signal_entry(&entry, libc::SIGKILL);
        let by_sid = signal_session(7, libc::SIGKILL);
        let missing = signal_session(8, libc::SIGKILL);
        let alive = child.try_wait().unwrap().is_none();
        assert_eq!(by_entry, Err(SignalError::Gone));
        assert_eq!(by_sid, Err(SignalError::Gone));
        assert_eq!(missing, Err(SignalError::Gone));
        assert!(alive, "the process the entry named was not signalled");
    }
}
