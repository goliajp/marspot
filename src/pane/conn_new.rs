//! Putting an `L3Conn` together, from a session this core started or
//! one it found running.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;

use super::{L3Conn, L3Process, L3Spawn, new_control_writer};
use crate::grid::Grid;
use crate::grid_shm::GridShmReader;

impl L3Conn {
    /// RFC-003 Amendment 7 step 4: assemble an L3Conn from pieces we
    /// reattached to (we didn't fork; the L3 was spawned by a prior
    /// L2 that has since swapped).  Same shape as `new` except the
    /// process handle wraps a foreign pid we don't own.
    pub fn reattach(
        pid: i32,
        control: UnixStream,
        reader: GridShmReader,
        regions: crate::grid_shm::SessionRegions,
        selection_rx: Receiver<(u32, String)>,
        session_id: u64,
    ) -> Self {
        let (cols, rows) = (reader.cols(), reader.rows());
        let grid = Grid::new(cols, rows);
        Self {
            child: L3Process::Reattached { pid },
            control: new_control_writer(control),
            reader,
            session_id,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            mouse_tracking_active: false,
            alt_screen_active: false,
            alt_scroll_active: false,
            mouse_sgr_active: false,
            mouse_drag_wanted: false,
            mouse_any_motion_wanted: false,
            pointer_gated: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            shell_ended: false,
            frame: Default::default(),
            remap: Vec::new(),
            scratch_wrapped: Vec::new(),
            cluster_table: Default::default(),
            clusters_tried_at: None,
            regions,
            moving: false,
            selection_rx,
            selection_seq: 0,
        }
    }

    /// Assemble from already-spawned pieces.  The container does the
    /// spawn (socketpair + shm region + `Command`); this is pure
    /// assembly so `Pane`/`PaneBackend` stay free of process-launch glue.
    pub fn new(spawn: L3Spawn, session_id: u64) -> Self {
        let (cols, rows) = (spawn.reader.cols(), spawn.reader.rows());
        let grid = Grid::new(cols, rows);
        Self {
            child: L3Process::Spawned(spawn.child),
            control: new_control_writer(spawn.control),
            reader: spawn.reader,
            session_id,
            grid,
            cursor_visible: true,
            app_cursor_keys: false,
            mouse_tracking_active: false,
            alt_screen_active: false,
            alt_scroll_active: false,
            mouse_sgr_active: false,
            mouse_drag_wanted: false,
            mouse_any_motion_wanted: false,
            pointer_gated: false,
            bracketed_paste: false,
            last_seq: 0,
            req_cols: cols,
            req_rows: rows,
            req_view_offset: 0,
            snap_scrollback_len: 0,
            exited: false,
            shell_ended: false,
            frame: Default::default(),
            remap: Vec::new(),
            scratch_wrapped: Vec::new(),
            cluster_table: Default::default(),
            clusters_tried_at: None,
            regions: spawn.regions,
            moving: false,
            selection_rx: spawn.selection_rx,
            selection_seq: 0,
        }
    }
}
