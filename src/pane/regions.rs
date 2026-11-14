//! Which shared regions an L3 pane reads, and giving its session a new
//! grid region when the one it publishes into cannot be shown.
//!
//! A session is never ended for its region.  A region that is gone, or
//! that turns out to be another session's, is replaced: the core
//! creates a new one, shows that, and asks the session to publish into
//! it.  The pane is empty for as long as the session takes to move.
//!
//! Also here: taking a new control stream after a reconnect, which is
//! when a session's new image says where it publishes.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;

use super::{L3Conn, PaneBackend, new_control_writer};
use crate::grid::Grid;
use crate::grid_shm::{ClusterTableReader, GridShmReader, SessionRegions};
use crate::shell_proto::{Frame, MsgType};
use marspot_term::{lx_event, lx_warn};

impl PaneBackend {
    /// Hot-swap an L3 pane's control stream (after a reader-loop EOF
    /// → main loop reconnects).  No-op on non-L3 backends.
    pub fn swap_l3_control(
        &mut self,
        new_control: UnixStream,
        selection_rx: Receiver<(u32, String)>,
    ) {
        if let PaneBackend::L3(c) = self {
            c.swap_control(new_control, selection_rx);
        }
    }

    /// The pane's session said which grid region it publishes into --
    /// see [`L3Conn::session_publishes`].  No-op on non-L3 backends.
    pub fn l3_session_publishes(&mut self, payload: &[u8]) {
        if let PaneBackend::L3(c) = self {
            c.session_publishes(payload);
        }
    }
}

impl L3Conn {
    /// A pane on session `session_id` with no region to show yet: the
    /// session is alive and answering, and what it publishes into
    /// cannot be opened or is not its own.  `cols` and `rows` size the
    /// new region; the session publishes its own dims into it.
    ///
    /// `from_record` is for a session that has to update before it
    /// can move, which takes as long as the update does: until its
    /// first frame arrives the pane shows the screen its record
    /// replays to.
    pub fn rehoused(
        pid: i32,
        control: UnixStream,
        selection_rx: Receiver<(u32, String)>,
        session_id: u64,
        (cols, rows): (u16, u16),
        from_record: bool,
    ) -> io::Result<Self> {
        let (reader, regions) = fresh_region(session_id, cols, rows)?;
        let mut conn = Self::reattach(pid, control, reader, regions, selection_rx, session_id);
        conn.moving = true;
        conn.ask_to_move();
        if from_record && let Ok(term) = crate::pane_read::replayed(session_id, cols, rows) {
            conn.show(term.grid());
        }
        Ok(conn)
    }

    /// Put `screen` in the mirror, to be replaced by the next frame.
    fn show(&mut self, screen: &Grid) {
        let (cols, rows) = (screen.cols(), screen.rows());
        self.grid = Grid::new(cols, rows);
        let mut clusters = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                let cell = screen.cell(col, row);
                self.grid.set_cell(col, row, cell);
                if let Some(text) = screen.cluster_text(&cell) {
                    clusters.push((col, row, text));
                }
            }
        }
        self.grid.mirror_clusters(clusters.into_iter());
        let (col, row) = screen.cursor();
        self.grid.set_cursor(col, row);
    }

    /// Replace this L3Conn's L2↔L3 control stream after a backend
    /// reconnect (`l3_reader_loop` saw EOF, main loop ran
    /// `wait_and_connect`, and now we hot-swap the write half so
    /// subsequent `forward_key`/`forward_resize`/etc go through the
    /// fresh stream).  Old stream's Drop closes the old fd.
    /// Both halves move together, and that is the whole point.
    ///
    /// This used to swap only `control` (the write half).  The reader
    /// half lives in a thread that owns the matching `selection_tx`, so
    /// after a reconnect the old thread was gone — its sender dropped —
    /// while `selection_rx` still pointed at that dead channel.
    /// `request_selection_text` then got `Disconnected` immediately and
    /// returned `None`, so **Cmd-C went permanently silent on that pane
    /// after any silent-update reconnect**, with no log to say why.
    /// `try_promote` (the swap path) always replaced both; this one
    /// didn't, and the asymmetry is what made it a bug.
    pub fn swap_control(
        &mut self,
        new_control: UnixStream,
        selection_rx: Receiver<(u32, String)>,
    ) {
        // Dropping the old writer ends its thread; anything still queued
        // for the dead socket goes with it, which is correct — those
        // frames were addressed to an L3 that no longer exists.
        self.control = new_control_writer(new_control);
        self.selection_rx = selection_rx;
    }

    pub(super) fn open_clusters(&self) -> io::Result<ClusterTableReader> {
        ClusterTableReader::open(&self.regions)
    }

    /// Stop showing the region shown now and give the session a new
    /// one.
    fn rehouse(&mut self) -> io::Result<()> {
        let (reader, regions) = fresh_region(self.session_id, self.grid.cols(), self.grid.rows())?;
        self.reader = reader;
        self.regions = regions;
        self.last_seq = 0;
        self.clusters_tried_at = None;
        self.moving = true;
        self.ask_to_move();
        Ok(())
    }

    /// A session that does not know the request drops it; its next
    /// image says where it publishes, and is asked again then.
    fn ask_to_move(&mut self) {
        let name = self.regions.grid_name().to_bytes().to_vec();
        let _ = self.control.send(Frame::new(MsgType::SessionRepublish, name));
    }

    /// The session said which grid region it publishes into: the
    /// region's serial, then its name.  When that is not the region
    /// shown here, the session is asked to move to it, or given a new
    /// one if the region shown was never made for a move.
    pub fn session_publishes(&mut self, payload: &[u8]) {
        let Some(serial) = payload.first_chunk::<8>().map(|b| u64::from_le_bytes(*b)) else {
            return;
        };
        let shown = self.reader.identity() == Some(self.regions.owner())
            && self.reader.serial() == Some(serial);
        if shown {
            if std::mem::take(&mut self.moving) {
                lx_event!(
                    "L3_REHOUSED",
                    "the session publishes into the region made for it",
                    session = self.session_id,
                    name = self.regions.grid_name().to_string_lossy()
                );
            }
        } else if self.moving {
            self.ask_to_move();
        } else if let Err(e) = self.rehouse() {
            lx_warn!("core.l3.rehouse_failed", &format!("{e}"), session = self.session_id);
        }
    }
}

/// A new, empty grid region for session `id` under the name unique to
/// it, and a reader on it.
fn fresh_region(id: u64, cols: u16, rows: u16) -> io::Result<(GridShmReader, SessionRegions)> {
    let regions = SessionRegions::for_session(id);
    let fd = regions.create(cols.max(1), rows.max(1))?;
    Ok((GridShmReader::from_fd(fd.as_raw_fd())?, regions))
}
