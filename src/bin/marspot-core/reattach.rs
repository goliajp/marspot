//! Taking over a session that outlived the core that started it.

use std::sync::mpsc::Sender;

use marspot::grid_shm::{GridShmReader, SessionRegions};
use marspot::pane::{L3Conn, Pane};
use marspot::session_signal::{UpdateAsk, ask_to_update, how_to_update, staged_session_build};
use marspot_term::session_registry::SessionEntry;
use marspot_term::{lx_event, lx_warn};

use super::{CoreEvent, l3_reader_loop};

/// Take over the live session `session_id`: connect to its socket and
/// show what it publishes.  The process is not ours and is never
/// started here.
///
/// An error means the pane cannot be had from the running process, and
/// the caller starts the session again in its place:
///   - it does not answer on its socket, or
///   - its grid region cannot be shown, it is from a build that cannot
///     be asked to publish into another, and no build that can is
///     staged for it to move to.
///
/// A session that answers and whose grid region is gone, or is another
/// session's, is given a new region and keeps running.
pub(super) fn reattach_l3_pane(
    session_id: u64,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let entry = marspot_term::session_registry::read_session_entry(session_id)?;
    // 4 s: reattach races a live L3 that may be mid-execv
    let control = marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(4),
    )?;
    let shown = shown_region(&entry);
    let must_update = match &shown {
        Ok(_) => false,
        Err(e) if entry.stamps_regions => {
            lx_event!(
                "L3_REHOUSE",
                "the session's grid region cannot be shown; giving it a new one",
                session_id = session_id,
                pid = entry.pid,
                shm_name = entry.shm_name,
                why = format!("{e}")
            );
            false
        }
        Err(e) => {
            let staged = staged_session_build();
            if how_to_update(&entry.build, staged.as_deref()) == UpdateAsk::Nothing {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("{e}; the session predates moving to another region and has no build to move to"),
                ));
            }
            lx_warn!(
                "core.reattach.waiting_for_session_update",
                &format!("{e}; the pane shows its record until the session has moved to the staged build"),
                session = session_id
            );
            true
        }
    };
    let reader_stream = control.try_clone()?;
    let tx = event_tx.clone();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel::<(u32, String)>();
    let conn = match shown {
        Ok((reader, regions)) => {
            L3Conn::reattach(entry.pid, control, reader, regions, selection_rx, session_id)
        }
        Err(_) => L3Conn::rehoused(
            entry.pid,
            control,
            selection_rx,
            session_id,
            (entry.cols, entry.rows),
            must_update,
        )?,
    };
    std::thread::spawn(move || l3_reader_loop(reader_stream, session_id, tx, selection_tx));
    lx_event!(
        "L3_REATTACHED",
        "took over surviving L3 from previous L2 image",
        session_id = session_id,
        pid = entry.pid,
        shm_name = entry.shm_name
    );
    let mut pane = Pane::new_l3(conn);
    if must_update {
        ask_to_update(&mut pane, staged_session_build().as_deref());
    }
    Ok(pane)
}

/// The grid region `entry`'s session publishes into, when it can be
/// opened and is that session's.
fn shown_region(entry: &SessionEntry) -> std::io::Result<(GridShmReader, SessionRegions)> {
    let regions = SessionRegions::of_entry(entry).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "the session names no grid region")
    })?;
    Ok((GridShmReader::open(&regions)?, regions))
}
