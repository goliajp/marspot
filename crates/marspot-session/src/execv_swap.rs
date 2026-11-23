//! The self-execv: this image hands its PTY, listener and control
//! stream to the image staged in `current/`.

use std::os::fd::RawFd;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use marspot_term::{lx_error, lx_event};

use crate::control_writer::ControlWriter;
use crate::handoff::{ENV_HANDOFF_MANIFEST, L3Handoff, handoff_manifest_path, write_handoff_manifest};
use crate::pane_ctl::{PaneCtl, local_mut};
use crate::snapshot_writer::SnapshotWriter;
use crate::{SessionEvent, SessionImpl, local_session, session_state_bin_path, uds_server};

pub(crate) fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 { return Err(std::io::Error::last_os_error()); }
        if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

pub(crate) fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 { return Err(std::io::Error::last_os_error()); }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Everything this image still owes before it hands over, once the
/// loop has ended and the control readers are stopped (they queue no
/// more; what they have not read stays in the socket for the next
/// image).  Input they queued is written; requests are answered as
/// restarting; the PTY's last output is parsed and counted, so that
/// the position the handoff carries is the one the bytelog has.
pub(crate) fn settle(
    session: &mut SessionImpl,
    events: &Receiver<SessionEvent>,
    pane_ctl: &mut PaneCtl,
    mut poke: Option<&mut ControlWriter>,
) {
    while let Ok(ev) = events.try_recv() {
        match ev {
            SessionEvent::Key(e, m) => {
                crate::keys::handle_key(session, e, m);
            }
            SessionEvent::Paste(text) => crate::keys::handle_paste(session, &text),
            SessionEvent::InjectInput(bytes) => {
                let _ = local_mut(session).write(&bytes);
            }
            SessionEvent::PaneControl(r) => pane_ctl.refuse_restarting(r, poke.as_deref_mut()),
            _ => {}
        }
    }
    local_mut(session).quiesce_for_handoff();
    pane_ctl.restarting(session, poke);
}

/// Stage the execv handoff and replace this process's image with
/// `current/marspot-session`; [`settle`] has run.  Returns Err only if
/// we couldn't even reach execv — on success the function does not
/// return.
pub(crate) fn do_l3_execv_swap(
    id: u64,
    local: local_session::LocalSession,
    mut listener: uds_server::SessionListener,
    poke: Option<ControlWriter>,
    snapshots: &SnapshotWriter,
    marks: crate::pane_ctl::Marks,
    shadow: crate::shadow::ShadowCfg,
) -> std::io::Result<()> {
    let started = Instant::now();
    let (master_fd, child_pid, terminal, cols, rows) = local.extract_for_handoff();

    // v5 LIVE snapshot (RFC-004 C.1 amendment): the TUI on this PTY
    // survives the execv, so an alt screen must come back verbatim —
    // the fold form here swapped the visible base out from under the
    // TUI's incremental repaints (claudecode's input box vanished
    // until its next full repaint).
    let body = terminal.serialize_snapshot_live();
    snapshots.write_last(session_state_bin_path(id), body);
    // F3+10 — flush the FileScrollback BufWriter tail to the kernel
    // page cache before execv.  Drop won't run on `libc::execv` so
    // anything sitting in the writer's 64 KB user-space buffer would
    // otherwise be lost to the next L3.  Snapshot v3 self-describing
    // index dedup is the safety net (it'll refill any remaining gap
    // from its RAM-ring section), but flushing keeps disk + RAM in
    // sync so the next L3's cold reads hit complete data immediately.
    terminal.grid().scrollback_flush_for_handoff();

    clear_cloexec(master_fd)?;
    let listen_fd = listener.prepare_for_execv()?;

    // Carry the adopted control stream across execv too, so L2's
    // reader doesn't see EOF.  `into_raw_fd` so the OwnedFd doesn't
    // close on drop after we've cleared CLOEXEC on it.
    let control_stream_fd: RawFd = match poke {
        Some(writer) => {
            let fd = writer.into_raw_fd();
            clear_cloexec(fd)?;
            fd
        }
        None => -1,
    };

    let manifest_path = handoff_manifest_path();
    let handoff = L3Handoff {
        master_fd, child_pid, listen_fd, cols, rows, control_stream_fd, marks, shadow,
    };
    write_handoff_manifest(&handoff, &manifest_path)?;

    let target = marspot_term::binary_tree::BinaryTree::default_for("marspot-session")?.current();
    let target_c = std::ffi::CString::new(target.to_string_lossy().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in target"))?;
    let argv: [*const libc::c_char; 2] = [target_c.as_ptr(), std::ptr::null()];

    unsafe { std::env::set_var(ENV_HANDOFF_MANIFEST, &manifest_path); }

    lx_event!(
        "L3_EXECV_INVOKE",
        "execv into new marspot-session image",
        session_id = id,
        target = target.display(),
        manifest = manifest_path.display(),
        master_fd = master_fd,
        listen_fd = listen_fd,
        control_stream_fd = control_stream_fd,
        child_pid = child_pid,
        elapsed_us = started.elapsed().as_micros()
    );

    // Drop listener so suppress_drop runs (mem::forget keeps the
    // listener fd open for the new image).
    std::mem::drop(listener);

    // Held across the exec and released by the new image once its own
    // handler is armed — see `set_sigterm_blocked`.
    marspot_term::signals::set_sigterm_blocked(true);
    unsafe { libc::execv(target_c.as_ptr(), argv.as_ptr()); }
    // Only reached when execv failed; this process keeps running, so
    // it must be able to receive SIGTERM again.
    marspot_term::signals::set_sigterm_blocked(false);
    let err = std::io::Error::last_os_error();
    lx_error!("l3.execv.failed", &format!("{err}"), target = target.display());
    unsafe { std::env::remove_var(ENV_HANDOFF_MANIFEST); }
    let _ = std::fs::remove_file(&manifest_path);
    Err(err)
}
