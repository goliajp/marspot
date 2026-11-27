//! Putting an `L3Conn` together, from a session this core started or
//! one it found running.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;

use super::{L3Conn, L3Process, L3Spawn};
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
            req_view_offset: Some(0),
            view_seq: 0,
            view_sync: Default::default(),
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
            reads_pane_control: false,
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
            req_view_offset: Some(0),
            view_seq: 0,
            view_sync: Default::default(),
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
            reads_pane_control: false,
        }
    }
}

pub(super) fn new_control_writer(s: UnixStream) -> marspot_term::frame_writer::FrameWriter {
    marspot_term::frame_writer::FrameWriter::new("l2-l3-writer", s, marspot_term::frame_writer::cap::CONTROL_SOCKET)
}

/// Tell the session which frames this core reads: only once it said it
/// reads the message (`SessionReadsFrameCells`), which each image does
/// on each connection.  A session from before the message would not
/// read it: its control reader blocks for the frame after one of a
/// type it does not know, past its check for an update, and the update
/// hangs until the core sends something else.
pub(super) fn say_frames_read(w: &marspot_term::frame_writer::FrameWriter) {
    let _ = w.send(crate::shell_proto::Frame::new(
        crate::shell_proto::MsgType::SessionFrameCells,
        crate::shell_proto::encode_frame_cells(crate::grid_shm::cells::VERSION_PACKED),
    ));
}

#[cfg(test)]
mod tests {
    use super::super::PaneBackend;
    use super::*;
    use crate::grid_shm::{GridShmWriter, SessionRegions};
    use crate::shell_proto::{Frame, MsgType, Next, encode_ping};
    use std::time::Duration;

    /// The types of what the core sent the session, up to a ping sent
    /// after them: the core writes from a thread of its own.
    fn sent(backend: &PaneBackend, session: &mut UnixStream) -> Vec<u32> {
        let PaneBackend::L3(c) = backend else {
            unreachable!()
        };
        c.control.send(Frame::new(MsgType::Ping, encode_ping(0)));
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut got = Vec::new();
        loop {
            match Frame::read_next(session)
                .expect("a frame")
                .expect("the core still there")
            {
                Next::Frame(f) if f.msg_type == MsgType::Ping => return got,
                Next::Frame(f) => got.push(f.msg_type as u32),
                Next::Unknown(t) => got.push(t),
            }
        }
    }

    /// 2026-11-16: a core told every session which frames it reads on
    /// connecting, and again on each `SessionPublishes`; six sessions
    /// from before the message hung their update in it.
    #[test]
    fn a_session_is_told_which_frames_the_core_reads_only_once_it_says_it_reads_that() {
        const CELLS: u32 = MsgType::SessionFrameCells as u32;
        let mut writer = GridShmWriter::create(20, 3).expect("shm region");
        writer.publish(&Grid::new(20, 3), 0, 0);
        // SAFETY: dup of an fd the writer holds open
        let fd = unsafe { libc::dup(writer.fd()) };
        assert!(fd >= 0, "dup of the shm fd");
        let reader = GridShmReader::from_fd(fd).expect("shm reader");
        let (control, mut session) = UnixStream::pair().expect("socketpair");
        let (_tx, selection_rx) = std::sync::mpsc::channel();
        let id = 0xFC_0000_0000 + std::process::id() as u64;
        // our own pid: `reattach` never signals it
        let mut conn = L3Conn::reattach(
            std::process::id() as i32,
            control,
            reader,
            SessionRegions::for_session(id),
            selection_rx,
            id,
        );
        assert!(conn.poll(), "the frame reaches the mirror");
        let mut backend = PaneBackend::L3(conn);
        assert!(
            !sent(&backend, &mut session).contains(&CELLS),
            "told on connecting"
        );

        backend.l3_session_says(&Frame::new(MsgType::SessionReadsFrameCells, Vec::new()));
        assert_eq!(sent(&backend, &mut session), [CELLS]);
    }
}
