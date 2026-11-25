//! The control socket a core hands a cold-started session: the fd
//! number in `MARSPOT_SHELL_CONTROL_FD`.
//!
//! Both the number and the descriptor are this image's alone.  An
//! update execs a new image with the environment and every descriptor
//! not marked close-on-exec, and that image takes its control stream
//! from the handoff, so a variable or descriptor left behind would
//! cross into it with nobody owning it.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;

use marspot_term::shell_proto::ENV_CONTROL_FD;
use marspot_term::{lx_error, lx_info, lx_warn};

use crate::SessionEvent;
use crate::control_reader::ControlReaders;
use crate::control_writer::ControlWriter;
use crate::execv_swap::set_cloexec;

/// What the environment said, read before logging is up.
pub(crate) enum Inherited {
    /// standalone: no control socket
    None,
    Fd(RawFd),
    /// the variable was set but named no usable descriptor
    Bad(String),
}

/// Read the variable, mark its descriptor close-on-exec and remove the
/// variable.  Called first thing in main, before any thread starts:
/// removing a variable races every other thread reading the environment.
pub(crate) fn take_env() -> Inherited {
    let Some(value) = std::env::var_os(ENV_CONTROL_FD) else {
        return Inherited::None;
    };
    // SAFETY: no other thread exists yet (see above)
    unsafe { std::env::remove_var(ENV_CONTROL_FD) };
    match value.to_str().and_then(|v| v.parse::<RawFd>().ok()).ok_or(()) {
        Ok(fd) => match set_cloexec(fd) {
            Ok(()) => Inherited::Fd(fd),
            Err(e) => Inherited::Bad(format!("fd {fd}: {e}")),
        },
        Err(()) => Inherited::Bad(format!("not an fd number: {value:?}")),
    }
}

/// A resumed image takes only the handoff's stream.  Its environment
/// carries the variable only when the image before it was a build that
/// left it there; the descriptor it names is marked close-on-exec by
/// now and leaves with the next update.
pub(crate) fn ignore_on_resume(inherited: Inherited) {
    if let Inherited::Fd(fd) = inherited {
        lx_warn!(
            "session.control_fd.env_on_resume",
            "the env named a control fd after an execv; ignored",
            fd = fd
        );
    }
}

/// If L2 handed us a control socket, spawn a reader thread that decodes
/// forwarded `KeyEvent` frames and feeds them to the main loop, and
/// return a write half so the loop can poke L2 with `GridReady` after
/// each publish. EOF / error on the read side means L2/core is gone, so
/// the main loop exits (`CoreGone`): the surviving session is
/// re-attached by the next core, so an orphaned engine would only leak.
pub(crate) fn setup_control_socket(
    inherited: Inherited,
    readers: &mut ControlReaders,
    tx: Sender<SessionEvent>,
) -> Option<ControlWriter> {
    let fd = match inherited {
        Inherited::None => return None,
        Inherited::Bad(why) => {
            lx_warn!("session.control_fd.parse_failed", &why, env_var = ENV_CONTROL_FD);
            return None;
        }
        Inherited::Fd(fd) => fd,
    };
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            lx_error!("session.control_socket.try_clone_failed", &format!("{e}"), fd = fd);
            return None;
        }
    };
    lx_info!("session.control_socket.attached", "control socket fd inherited", fd = fd);
    // generation 0: if it dies, no client is left (not a stale-reader race)
    readers.spawn(stream, tx, 0);
    match ControlWriter::new(writer) {
        Ok(w) => Some(w),
        Err((e, _)) => {
            // without a writer thread the only alternative is writing
            // inline, which wedged; running pokeless keeps the pane
            // responsive while L2 polls
            lx_error!(
                "session.control_socket.writer_spawn_failed",
                &format!("{e}; running without a poke channel"),
                fd = fd
            );
            None
        }
    }
}
