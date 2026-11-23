//! A session that is over for good, closed by the app or with its shell
//! ended, removes its own directory, as the last thing it does.
//!
//! Nobody else can: until this process has stopped writing, a file one
//! of its threads makes while the directory is being emptied is left
//! behind.  So the threads that write there are ended first.  The
//! directory leaves the registry before that, so that a core starting
//! meanwhile takes the session for neither alive nor crashed.

use std::path::PathBuf;

use marspot_term::{lx_event, lx_warn, session_registry};

use crate::local_session::LocalSession;
use crate::snapshot_writer::SnapshotWriter;
use crate::uds_server::SessionListener;
use crate::{SessionImpl, regions, session_state_bin_path, shell_end};

/// This session's directory, out of the registry, for the caller to
/// delete.  `None` when the directory is not this session's (any more).
pub(crate) fn take_own_dir(sid: u64) -> Option<PathBuf> {
    let entry = session_registry::read_session_entry(sid);
    if !entry.is_ok_and(|e| e.pid == std::process::id() as i32) {
        return None;
    }
    session_registry::take_session_dir(sid)
        .inspect_err(|e| lx_warn!("session.dir.take_failed", &format!("{e}"), session_id = sid))
        .ok()
        .flatten()
}

/// SIGHUP: the session ends, and its history with it only when the app
/// closed it.
pub(crate) fn hung_up(session: SessionImpl, listener: Option<SessionListener>, files: &SnapshotWriter) {
    let sid = session.id();
    if session_registry::is_closed(sid) {
        return finish(session, listener, files, true);
    }
    let SessionImpl::Local(local) = session;
    shut_down(sid, &local, files);
}

/// End a session that was closed, or whose directory was taken away,
/// while no main loop ran to act on it: between two images, where
/// nothing of this process writes to the directory.
pub(crate) fn end_if_over(sid: u64, shell: i32, before: impl FnOnce()) {
    let gone = matches!(session_registry::session_dir(sid).try_exists(), Ok(false));
    if !session_registry::is_closed(sid) && !gone {
        return;
    }
    before();
    regions::release(sid);
    let dir = take_own_dir(sid);
    shell_end::end_with_pane(sid, shell);
    removed(sid, dir, true);
    std::process::exit(0)
}

/// `hang_up`: the shell is still there and ends with the pane.
pub(crate) fn finish(session: SessionImpl, listener: Option<SessionListener>, files: &SnapshotWriter, hang_up: bool) {
    let sid = session.id();
    regions::release(sid);
    let dir = listener.and_then(SessionListener::take_dir);
    if hang_up {
        shell_end::end_with_pane(sid, session.child_pid());
    }
    // ends the history's and the byte log's disk writers, then what
    // they left to the files thread
    drop(session);
    files.settle();
    removed(sid, dir, hang_up);
}

fn removed(sid: u64, dir: Option<PathBuf>, hang_up: bool) {
    match dir.as_ref().map(std::fs::remove_dir_all) {
        Some(Ok(())) => lx_event!(
            "SESSION_DIR_REMOVED",
            "the session is over; its directory is removed",
            session_id = sid,
            closed = hang_up as u32
        ),
        Some(Err(e)) => lx_warn!("session.dir.remove_failed", &format!("{e}"), session_id = sid),
        None => lx_event!(
            "SESSION_DIR_NOT_OURS",
            "the session is over; it has no directory of its own to remove",
            session_id = sid
        ),
    }
}

/// The session's process ends and the session does not: the machine is
/// shutting down, or a hangup came that was no close.  Its directory
/// and its screen stay, to be resurrected from.
pub(crate) fn shut_down(sid: u64, local: &LocalSession, files: &SnapshotWriter) {
    // a directory that is another session's by now is not written to
    let theirs = session_registry::read_session_entry(sid).is_ok_and(|e| e.pid != std::process::id() as i32);
    if !theirs {
        files.write_last(session_state_bin_path(sid), local.terminal().serialize_snapshot());
        local.terminal().grid().scrollback_flush_for_handoff();
    }
    lx_event!(
        "L3_SHUTDOWN_EXIT",
        "clean exit: the session's history is kept",
        session_id = sid,
        pid = std::process::id()
    );
    regions::release(sid);
    shell_end::end_with_pane(sid, local.child_pid());
}
