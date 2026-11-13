//! The shared regions this session publishes through: which they are
//! at start, and moving to another grid region when the core asks.

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use marspot_term::grid_shm::{self, ENV_SHM_FD, GridShmWriter, Identity, SessionRegions};
use marspot_term::shell_proto::{Frame, MsgType};
use marspot_term::{lx_error, lx_event, lx_info, lx_warn};

use crate::control_writer::ControlWriter;
use crate::frames::Frames;
use crate::{INITIAL_COLS, INITIAL_ROWS, SessionImpl};

/// The grid region's name, as the core passed it and the registry
/// entry repeats it.
const ENV_SHM_NAME: &str = "MARSPOT_SHM_NAME";

/// Bring up the grid framebuffer and report its geometry. When L2 owns
/// the region (`MARSPOT_SHM_FD` set) we take the writer role on the
/// inherited fd and adopt the region's dims; standalone we self-create
/// at the placeholder geometry. The returned `(cols, rows)` is what the
/// session must be sized to so its published grid fits the region.
pub(crate) fn setup_shm() -> (GridShmWriter, u16, u16) {
    match std::env::var(ENV_SHM_FD) {
        Ok(s) => {
            let fd: RawFd = s.parse().unwrap_or_else(|_| {
                lx_error!(
                    "session.shm_fd.parse_failed",
                    "bad shm fd env",
                    env_var = ENV_SHM_FD,
                    value = s
                );
                std::process::exit(1);
            });
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            let w = GridShmWriter::from_fd(owned).unwrap_or_else(|e| {
                lx_error!("session.grid_shm.from_fd_failed", &format!("{e}"), fd = fd);
                std::process::exit(1);
            });
            let (c, r) = (w.cols(), w.rows());
            lx_info!(
                "session.grid_shm.attached",
                "grid framebuffer from inherited fd",
                fd = fd,
                cols = c,
                rows = r
            );
            (w, c, r)
        }
        Err(_) => {
            let w = GridShmWriter::create(INITIAL_COLS, INITIAL_ROWS).unwrap_or_else(|e| {
                lx_error!("session.grid_shm.create_failed", &format!("{e}"));
                std::process::exit(1);
            });
            lx_info!(
                "session.grid_shm.self_created",
                "grid framebuffer self-created",
                fd = w.fd(),
                cols = INITIAL_COLS,
                rows = INITIAL_ROWS
            );
            (w, INITIAL_COLS, INITIAL_ROWS)
        }
    }
}

fn grid_name() -> Option<CString> {
    let name = std::env::var(ENV_SHM_NAME).ok().filter(|n| !n.is_empty())?;
    CString::new(name).ok()
}

/// Write into the grid region that it is session `id`'s.  Before the
/// registry entry says this session does so: a reader that finds the
/// entry must find the identity.
pub(crate) fn sign(shm: &mut GridShmWriter, id: u64) {
    if !shm.stamp(Identity::of_session(id)) {
        lx_info!(
            "session.grid_shm.unsigned",
            "the grid region has no room for an identity; the core gives this session another",
            session_id = id
        );
    }
}

/// Publish cluster text and the window title beside the grid region.
/// A session whose grid region has no name publishes neither: nobody
/// could find them.
pub(crate) fn attach_side_regions(shm: &mut GridShmWriter, id: u64) {
    if let Some(name) = grid_name() {
        shm.attach_side_regions(&name, Identity::of_session(id));
    }
}

/// Move to the grid region `name`, which the core created for this
/// session, and say so.  The screen is published there at once, the
/// side regions follow the new name, and the next image of this
/// session inherits the new region.  A region that is not this
/// session's is left alone and nothing changes.
pub(crate) fn republish(
    frames: &mut Frames,
    session: &mut SessionImpl,
    name: &str,
    mut poke: Option<&mut ControlWriter>,
) {
    let id = session.id();
    let owner = Identity::of_session(id);
    let opened = CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
        .and_then(|c| Ok((grid_shm::open_region(&c)?, c)))
        .and_then(|(fd, c)| Ok((GridShmWriter::from_fd(fd)?, c)));
    let (mut shm, cname) = match opened {
        Ok(pair) => pair,
        Err(e) => {
            lx_warn!("l3.republish.open_failed", &format!("{e}"), session_id = id, name = name);
            return;
        }
    };
    if shm.identity() != Some(owner) {
        lx_warn!(
            "l3.republish.not_ours",
            "asked to publish into a region that is not this session's",
            session_id = id,
            name = name
        );
        return;
    }
    let left = grid_name();
    shm.inherit_side_regions(frames.shm());
    shm.attach_side_regions(&cname, owner);
    let new_fd = shm.fd();
    // the old writer closes the fd the next image would inherit
    drop(frames.replace_shm(shm));
    if let Some(inherited) = std::env::var(ENV_SHM_FD).ok().and_then(|s| s.parse::<RawFd>().ok()) {
        // dup2 leaves close-on-exec clear, as the core left the first
        if unsafe { libc::dup2(new_fd, inherited) } < 0 {
            lx_warn!(
                "l3.republish.dup_failed",
                &format!("{}", std::io::Error::last_os_error()),
                session_id = id
            );
        }
    }
    unsafe { std::env::set_var(ENV_SHM_NAME, name) };
    match marspot_term::session_registry::read_session_entry(id) {
        Ok(mut entry) => {
            entry.shm_name = name.to_string();
            if let Err(e) = marspot_term::session_registry::write_session_entry(&entry) {
                lx_warn!("l3.republish.entry_write_failed", &format!("{e}"), session_id = id);
            }
        }
        Err(e) => lx_warn!("l3.republish.entry_read_failed", &format!("{e}"), session_id = id),
    }
    // what this session left behind, where the names are still its own
    if let Some(left) = left.filter(|l| l.as_c_str() != cname.as_c_str()) {
        SessionRegions::new(left, id, true).delete_if_owned();
    }
    frames.publish(session, poke.as_mut().map(|w| &mut **w));
    announce(frames.shm(), poke);
    lx_event!(
        "L3_REPUBLISHED",
        "publishing into the grid region the core asked for",
        session_id = id,
        name = name
    );
}

/// Tell the core which grid region this session publishes into: its
/// serial, then its name.  A core that shows another one asks for a
/// move.
pub(crate) fn announce(shm: &GridShmWriter, poke: Option<&mut ControlWriter>) {
    if let (Some(w), Some(serial)) = (poke, shm.serial()) {
        let mut payload = serial.to_le_bytes().to_vec();
        payload.extend_from_slice(std::env::var(ENV_SHM_NAME).unwrap_or_default().as_bytes());
        let _ = w.send(Frame::new(MsgType::SessionPublishes, payload));
    }
}

/// The session is over and no image follows it: remove the names of
/// its regions, each only if the region under it still says it is
/// this session's.
pub(crate) fn release(id: u64) {
    if let Some(name) = grid_name() {
        SessionRegions::new(name, id, true).delete_if_owned();
    }
}
