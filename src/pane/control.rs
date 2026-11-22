//! Whether a pane's session can be handed the pane-control frames that
//! type into it and watch it, and handing them over.
//!
//! Each image of a session says it reads them (`SessionReadsPaneControl`,
//! right after its `SessionPublishes`), on each connection.  One that
//! has not said so is not sent them: an image from before them skips
//! the frame without answering, and its control reader then waits past
//! its update check for whatever frame comes next.

use super::PaneBackend;
use crate::shell_proto::Frame;

/// Where a pane-control request for one pane can go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlReach {
    /// No live session connection: a slot still starting, a
    /// placeholder, a local PTY, or a session that has exited.
    NotAttached,
    /// The session's current image never said it reads these frames.
    OldSession,
    /// The session reads them.
    Reads,
}

impl PaneBackend {
    pub fn pane_control_reach(&self) -> ControlReach {
        match self {
            PaneBackend::L3(c) if c.exited => ControlReach::NotAttached,
            PaneBackend::L3(c) if !c.reads_pane_control => ControlReach::OldSession,
            PaneBackend::L3(_) => ControlReach::Reads,
            _ => ControlReach::NotAttached,
        }
    }

    /// Queue `frame` for the session unchanged.  `false`: not a session
    /// pane, or its queue refused the frame.
    pub fn send_pane_control(&self, frame: Frame) -> bool {
        match self {
            PaneBackend::L3(c) => c.control.send(frame),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{L3Conn, PaneBackend};
    use super::*;
    use crate::grid::Grid;
    use crate::grid_shm::{GridShmReader, GridShmWriter, SessionRegions};
    use crate::shell_proto::{MsgType, Next, encode_ping};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn conn(control: UnixStream) -> (PaneBackend, GridShmWriter) {
        let mut writer = GridShmWriter::create(20, 3).expect("shm region");
        writer.publish(&Grid::new(20, 3), 0, 0);
        // SAFETY: dup of an fd the writer holds open
        let fd = unsafe { libc::dup(writer.fd()) };
        assert!(fd >= 0, "dup of the shm fd");
        let reader = GridShmReader::from_fd(fd).expect("shm reader");
        let (_tx, selection_rx) = std::sync::mpsc::channel();
        let id = 0xFD_0000_0000 + std::process::id() as u64;
        // our own pid: `reattach` never signals it
        let c = L3Conn::reattach(
            std::process::id() as i32,
            control,
            reader,
            SessionRegions::for_session(id),
            selection_rx,
            id,
        );
        (PaneBackend::L3(c), writer)
    }

    /// What the core sent the session, up to a ping sent after it.
    fn sent(backend: &PaneBackend, session: &mut UnixStream) -> Vec<Frame> {
        assert!(backend.send_pane_control(Frame::new(MsgType::Ping, encode_ping(0))));
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
                Next::Frame(f) => got.push(f),
                Next::Unknown(t) => panic!("the core sent an unknown type {t}"),
            }
        }
    }

    fn says(backend: &mut PaneBackend, ty: MsgType) {
        backend.l3_session_says(&Frame::new(ty, Vec::new()));
    }

    #[test]
    fn typing_and_watch_frames_are_for_an_image_that_said_it_reads_them() {
        let (control, mut session) = UnixStream::pair().expect("socketpair");
        let (mut backend, _writer) = conn(control);
        assert_eq!(
            backend.pane_control_reach(),
            ControlReach::OldSession,
            "before it said so"
        );

        says(&mut backend, MsgType::SessionReadsPaneControl);
        assert_eq!(backend.pane_control_reach(), ControlReach::Reads);
        let payload = vec![7u8; 40];
        assert!(backend.send_pane_control(Frame::new(MsgType::PaneType, payload.clone())));
        let got = sent(&backend, &mut session);
        assert_eq!(got.len(), 1, "one frame reached the session");
        assert_eq!(got[0].msg_type, MsgType::PaneType);
        assert_eq!(got[0].payload, payload, "unchanged");

        // a new image publishes first, and announces itself after
        says(&mut backend, MsgType::SessionPublishes);
        assert_eq!(
            backend.pane_control_reach(),
            ControlReach::OldSession,
            "after a re-image"
        );
        says(&mut backend, MsgType::SessionReadsPaneControl);
        assert_eq!(backend.pane_control_reach(), ControlReach::Reads);
    }

    #[test]
    fn typing_and_watch_reach_resets_with_a_new_control_stream() {
        let (control, _session) = UnixStream::pair().expect("socketpair");
        let (mut backend, _writer) = conn(control);
        says(&mut backend, MsgType::SessionReadsPaneControl);
        let (fresh, _peer) = UnixStream::pair().expect("socketpair");
        let (_tx, rx) = std::sync::mpsc::channel();
        backend.swap_l3_control(fresh, rx);
        assert_eq!(backend.pane_control_reach(), ControlReach::OldSession);
    }

    #[test]
    fn typing_and_watch_have_no_reach_into_a_placeholder() {
        let backend = PaneBackend::Vacant(super::super::VacantPane::new(1, 20, 3));
        assert_eq!(backend.pane_control_reach(), ControlReach::NotAttached);
        assert!(!backend.send_pane_control(Frame::new(MsgType::PaneWatch, Vec::new())));
    }
}
