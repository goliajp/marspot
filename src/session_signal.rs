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

/// How to ask the L3 behind an entry to move to the `staged` build.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateAsk {
    /// It understands `SessionUpdate`; repeating the frame is harmless.
    Frame,
    /// It predates the frame and takes an update as one SIGTERM.
    Sigterm,
    /// It runs that build already, or nothing readable is staged.
    Nothing,
}

/// `entry_build` is the entry's `build` (empty from an L3 that predates
/// it), `staged` the build in `current/`.
pub fn how_to_update(entry_build: &str, staged: Option<&str>) -> UpdateAsk {
    match staged {
        None => UpdateAsk::Nothing,
        Some(s) if s == entry_build => UpdateAsk::Nothing,
        Some(_) if entry_build.is_empty() => UpdateAsk::Sigterm,
        Some(_) => UpdateAsk::Frame,
    }
}

/// The build in `current/marspot-session`, if one reads.
pub fn staged_session_build() -> Option<String> {
    let tree = marspot_term::binary_tree::BinaryTree::default_for("marspot-session").ok()?;
    marspot_term::build::of_file(&tree.current())
}

/// Ask the L3 behind `pane` to move to `staged`, the way it understands:
/// a `SessionUpdate` frame, or one SIGTERM from an L3 that predates it.
/// Nothing for an L3 already on that build -- asking twice must never
/// end or re-exec a pane.  True when it was asked.
pub fn ask_to_update(pane: &mut crate::pane::Pane, staged: Option<&str>) -> bool {
    let Some(sid) = pane.session().l3_session_id() else {
        return false;
    };
    let Ok(entry) = read_session_entry(sid) else {
        return false;
    };
    let ask = how_to_update(&entry.build, staged);
    let asked = match ask {
        UpdateAsk::Frame => pane.session_mut().forward_session_update(),
        UpdateAsk::Sigterm => signal_entry(&entry, libc::SIGTERM).is_ok(),
        UpdateAsk::Nothing => false,
    };
    marspot_term::lx_event!(
        "L3_SWAP_ASK",
        "asked an L3 to move to current/marspot-session",
        session = sid,
        pid = entry.pid,
        how = format!("{ask:?}"),
        sent = asked as u8
    );
    asked
}

/// [`ask_to_update`] for every live L3 pane of `panes`, against the
/// build staged now.  Returns how many were asked.
pub fn ask_all_to_update<'a>(panes: impl Iterator<Item = &'a mut crate::pane::Pane>) -> usize {
    let staged = staged_session_build();
    panes
        .filter(|p| p.is_l3() && !p.is_exited())
        .map(|p| ask_to_update(p, staged.as_deref()) as usize)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::session_registry::write_session_entry;

    /// A pane already on the staged build is left alone, whoever asks
    /// and however often: a second request must not end or re-exec it.
    #[test]
    fn each_l3_is_asked_the_way_it_understands_and_only_if_it_must_move() {
        let new = "MARSPOT_FP=b|2";
        assert_eq!(how_to_update("MARSPOT_FP=a|1", Some(new)), UpdateAsk::Frame);
        assert_eq!(how_to_update("", Some(new)), UpdateAsk::Sigterm, "an L3 from before `build`");
        assert_eq!(how_to_update(new, Some(new)), UpdateAsk::Nothing);
        assert_eq!(how_to_update("", None), UpdateAsk::Nothing, "nothing staged");
        assert_eq!(how_to_update("MARSPOT_FP=a|1", None), UpdateAsk::Nothing);
    }

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
            build: String::new(),
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
